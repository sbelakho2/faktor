//! faktor-cli — `serve`, `run`, `doctor`, `sessions`, `acp` (spec §34, §42,
//! §43, and the ACP agent server over the daemon).
//!
//! `serve --port 0` prints the exact frozen startup line
//! `faktor server listening on http://127.0.0.1:<port>` so the
//! extension connects exactly as it did to the old CLI. When the release
//! bootstrap launcher started this process it also exported
//! `FAKTOR_RELEASE_DIGEST`; the daemon then prints the ONE frozen child
//! attestation line `faktor release digest=<64 lowercase hex>` BEFORE the
//! startup line (see [`emit_release_digest_attestation`]). Nothing else goes
//! to stdout. Auth comes from the frontend-generated `FAKTOR_SERVER_PASSWORD`
//! environment variable; the daemon never prints it.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use evidence::RepoEvidence;
use faktor_acp::{AcpBackend, AcpServer};
use faktor_agent::{AgentDeps, AgentRuntime, ToolCallMode, ToolRegistry};
use faktor_core::id::SessionId;
use faktor_core::time::SystemClock;
use faktor_core::CapabilitySet;
use faktor_provider::egress::{
    EgressAddressPolicy, HttpTransport, OutboundScanConfig, PolicyCheckedHttpTransport,
};
use faktor_provider::{Provider, ProviderRegistry};
use faktor_security::registry::SecretRegistry;
use faktor_server::permission::ChannelPermissionRequester;
use faktor_server::{ServerDeps, ServerPassword};
use faktor_session::SessionManager;
use faktor_terminal::{ProcessOwner, ProcessSupervisor};
use serde_json::{json, Value};

mod billing_debits;
mod billing_report;
mod bootstrap;
mod config;
mod embeddings;
mod evidence;
mod graph;
mod mcp_bridge;
mod payload;
mod scm_daemon;
mod sso_auth;
#[cfg(test)]
mod test_http;
mod tools;
mod tools_market;
mod worker_node;

use graph::DaemonGraph;

#[derive(Parser)]
#[command(
    name = "faktor",
    version,
    about = "Faktor — native Rust agent engine, daemon and CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon; prints the Faktor startup line on stdout.
    Serve {
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        #[arg(long)]
        config: Option<String>,
    },
    /// Headless: create a session and run one prompt.
    Run {
        prompt: String,
        #[arg(long, default_value = "fake")]
        provider: String,
        #[arg(long, default_value = "default")]
        model: String,
        #[arg(long, default_value = ".")]
        workspace: String,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Self-check: storage, CAS, permissions, providers.
    Doctor {
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        /// Run the full deep scan: complete store integrity check, CAS blob
        /// verification, global recovery-row scan, dangling CAS references,
        /// journal projection consistency and the audit invariants (dangling
        /// cost reservations, verification-record/task consistency, active-
        /// turn recoverable owners, orphan children, process ownership).
        /// Plain mode keeps the bounded quick checks.
        #[arg(long)]
        deep: bool,
        /// Optional daemon config to also audit: reports the resolved
        /// `[worker_plane]` deployment-boundary decision (bind, exposure,
        /// trusted-gateway acknowledgement) and surfaces a refused boundary
        /// as an issue. Absent = the check is skipped.
        #[arg(long)]
        config: Option<String>,
    },
    /// ACP (Agent Client Protocol) stdio agent server over the real daemon
    /// graph. Framed JSON-RPC on stdout ONLY; logs stay on stderr.
    Acp {
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Signed updater lifecycle (local parity with the control-plane
    /// routes): status, check, stage, apply, rollback, recover and the
    /// explicitly authorized downgrade below the anti-rollback floor. The
    /// `[updater]` section must be enabled; manifests are verified against
    /// its operator key allowlist and downloads ride the checked transport.
    Updater {
        #[command(subcommand)]
        action: UpdaterAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Enterprise retention/audit/admin plane (local parity with the
    /// `/native/enterprise/*` routes): status, audit export, guarded GC,
    /// deletion jobs, artifacts, settings and effective config. The
    /// `[enterprise]` section must be enabled; the local operator acts as
    /// an owner principal of the configured organization.
    Enterprise {
        #[command(subcommand)]
        action: EnterpriseAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Remote worker mode: register, claim, execute and submit remote jobs
    /// through a control plane. The `[worker_node]` section must be enabled
    /// (disabled by default: the entry refuses before any effect).
    Worker {
        #[command(subcommand)]
        action: WorkerAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// List sessions.
    Sessions {
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Stable bootstrap launcher: resolve the activated release through the
    /// authenticated install pointer (`versions/<release-id>/faktor`), verify
    /// its signed manifest and digest, and exec exactly those bytes. Refuses
    /// (exit 3) on a missing pointer, an unsigned/tampered manifest or a
    /// digest mismatch; never falls back to another binary.
    Bootstrap {
        /// The install root holding `current`, `launcher` and `versions/`.
        #[arg(long, default_value = "~/.faktor/install")]
        install_root: String,
        /// Everything after `--` is forwarded verbatim to the release binary.
        #[arg(last = true, allow_hyphen_values = true)]
        exec_args: Vec<String>,
    },
    /// Print the running build report as JSON: version, executable path, the
    /// sha256 of the RUNNING executable, and the release id/digest the
    /// bootstrap launcher verified (absent when the binary was started
    /// directly). Supervisors use this to observe which artifact is live.
    Build,
    /// Faktor Acquire local admin (never a model tool): `doctor`, `status`,
    /// `login <source>`, `logout <source>`, `clear-cache`. Login opens the
    /// headed dedicated profile browser; credentials never enter the model
    /// context. The `[commerce]` section must be enabled for every action
    /// except reading the disabled `doctor`/`status` report.
    Commerce {
        #[command(subcommand)]
        action: CommerceAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
}

/// The local Faktor Acquire subcommands (`faktor commerce <action>`).
#[derive(Subcommand)]
enum CommerceAction {
    /// Per-source configured/auth/quota/profile/browser/extraction/
    /// verification status (no LLM anywhere).
    Doctor,
    /// Local store/service status.
    Status,
    /// Open the headed dedicated profile browser of a source.
    Login {
        /// The source id (`1688`, `alibaba`, `lcsc`, `mouser`, `digikey`).
        source: String,
    },
    /// Remove a source's dedicated browser profile state.
    Logout {
        /// The source id (`1688`, `alibaba`).
        source: String,
    },
    /// Drop the whole acquisition cache.
    ClearCache,
}

/// The worker-node local subcommands (`faktor worker <action>`).
#[derive(Subcommand)]
enum WorkerAction {
    /// Run the bounded worker loop: register once, then claim/execute/submit
    /// until the iteration budget or the stop condition. Prints one JSON
    /// tally line on stdout.
    Run {
        /// Loop budget (bounded by the worker crate; default 1 = one pass).
        #[arg(long)]
        iterations: Option<u32>,
    },
}

/// The enterprise local-parity subcommands (`faktor enterprise <action>`).
#[derive(Subcommand)]
enum EnterpriseAction {
    /// Retention class table, audit head seq, artifact/job counts.
    Status,
    /// Export one cursor page of the enterprise audit ledger.
    Audit {
        /// Resume after this seq (exclusive).
        #[arg(long)]
        after: Option<i64>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    /// Run one guarded GC pass over the organization's artifacts (protected
    /// digests are refused at the scanner AND at the CAS).
    Gc {
        #[arg(long, default_value_t = 100)]
        limit: u64,
    },
    /// Retention artifact operations.
    Artifact {
        #[command(subcommand)]
        action: EnterpriseArtifactAction,
    },
    /// Deletion-job operations.
    Deletion {
        #[command(subcommand)]
        action: EnterpriseDeletionAction,
    },
    /// Show the organization's admin settings.
    Settings,
    /// Resolve the configured layered configuration and print its
    /// attributable digest.
    Config,
}

/// Retention artifact subcommands.
#[derive(Subcommand)]
enum EnterpriseArtifactAction {
    /// List one cursor page of artifacts.
    List {
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u64,
    },
    /// Register one artifact row.
    Register {
        #[arg(long)]
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        size: u64,
        #[arg(long)]
        retention_class: String,
        #[arg(long)]
        ttl_ms: Option<i64>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        task: Option<u64>,
        #[arg(long)]
        owner: Option<String>,
    },
    /// Promote one expired artifact to the eligible state.
    Eligible {
        #[arg(long)]
        id: String,
    },
}

/// Deletion-job subcommands.
#[derive(Subcommand)]
enum EnterpriseDeletionAction {
    /// Start (or return) the deterministic deletion job for a scope.
    Start {
        #[arg(long, default_value = "organization")]
        scope: String,
        #[arg(long)]
        user: Option<String>,
    },
    /// Show one deletion job.
    Show {
        #[arg(long)]
        id: String,
    },
    /// Advance the job by exactly one durable step.
    Advance {
        #[arg(long)]
        id: String,
    },
}

/// The local updater subcommands (`faktor updater <action>`).
#[derive(Subcommand)]
enum UpdaterAction {
    /// Show the installed version, the staged operation and the durable
    /// operation history.
    Status,
    /// Verify a signed manifest and check it against the running components.
    Check {
        /// Path to the `faktor-update/v1` manifest JSON.
        #[arg(long)]
        manifest: String,
        /// The running VS Code panel version (omit when not attached).
        #[arg(long)]
        vscode: Option<String>,
        /// The running JetBrains panel version (omit when not attached).
        #[arg(long)]
        jetbrains: Option<String>,
    },
    /// Download, verify and publish the host's artifact (the install is not
    /// touched).
    Stage {
        #[arg(long)]
        manifest: String,
        /// Idempotency key for a replayable stage.
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        vscode: Option<String>,
        #[arg(long)]
        jetbrains: Option<String>,
        /// Also materialize the IMMUTABLE release layout
        /// (`versions/<release-id>/{faktor,manifest}` + `launcher` +
        /// `trusted-keys.json`): the bootstrap launcher then controls which
        /// binary runs. Without this flag the legacy content-addressed stage
        /// is byte-identical.
        #[arg(long)]
        release: bool,
    },
    /// Swap the staged artifact in and run the doctor-style health probe;
    /// a probe failure rolls back automatically.
    Apply {
        #[arg(long)]
        confirm: bool,
    },
    /// Activate a staged RELEASE: record-first atomic pointer swap to
    /// `versions/<release-id>/faktor`, then the filesystem probe. The apply
    /// row stays RUNNING until the restarted process's digest is observed:
    /// this command alone never proves which binary runs (that is exactly
    /// the defect the immutable layout closes).
    Activate {
        #[arg(long)]
        confirm: bool,
    },
    /// Finalize a pending release activation: `--digest` is the digest the
    /// RESTARTED process reported (`faktor build` / the health `version`).
    /// It must equal the activated digest; then the doctor probe runs and
    /// the apply row becomes applied. A probe failure restores the previous
    /// pointer.
    Finalize {
        /// The release digest the running process reported.
        #[arg(long)]
        digest: String,
        #[arg(long)]
        confirm: bool,
    },
    /// Abort a pending release activation: restore the exact previous
    /// pointer (the caller then restarts the daemon through the launcher,
    /// which names the previous release again).
    Abort {
        #[arg(long)]
        confirm: bool,
    },
    /// Explicitly roll back to the artifact the last applied update replaced.
    Rollback {
        #[arg(long)]
        confirm: bool,
    },
    /// The authorized downgrade below the durable anti-rollback floor: install
    /// an OLDER signed manifest (it must carry an explicit signed
    /// `release_generation`). The local operator acts as the admin principal;
    /// the operation is recorded as a durable `downgrade` row and the
    /// high-water mark is reset only after the swap + health probe succeed.
    Downgrade {
        /// Path to the older `faktor-update/v1` manifest JSON.
        #[arg(long)]
        manifest: String,
        /// Idempotency key for the authorized downgrade.
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        vscode: Option<String>,
        #[arg(long)]
        jetbrains: Option<String>,
        /// The downgrade is never applied implicitly.
        #[arg(long)]
        confirm: bool,
    },
    /// Resolve crash residue: resume a swapped install or roll it back.
    Recover,
}

fn expand(p: &str) -> PathBuf {
    if p == "~" {
        return std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return std::env::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p));
    }
    PathBuf::from(p)
}

#[tokio::main]
async fn main() {
    // Bootstrap mode is detected BEFORE the CLI: a file named `launcher`
    // sitting in an install layout (or an explicit
    // FAKTOR_LAUNCHER_INSTALL_ROOT) resolves, authenticates and execs the
    // activated release. Without this entry the pointer could never control
    // which binary runs.
    if let Some(install_root) = bootstrap::launcher_invocation() {
        bootstrap::run_launcher(install_root, std::env::args_os().skip(1).collect());
    }
    // Logging goes to stderr: stdout is the frozen startup-line contract.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            port,
            data_dir,
            config,
        } => {
            serve(port, expand(&data_dir), config.map(|c| expand(&c))).await;
        }
        Command::Run {
            prompt,
            provider,
            model,
            workspace,
            data_dir,
        } => {
            run(
                prompt,
                &provider,
                &model,
                expand(&workspace),
                expand(&data_dir),
            )
            .await;
        }
        Command::Doctor {
            data_dir,
            deep,
            config,
        } => {
            doctor(expand(&data_dir), deep, config.map(|c| expand(&c))).await;
        }
        Command::Acp { data_dir } => {
            acp(expand(&data_dir)).await;
        }
        Command::Updater {
            action,
            data_dir,
            config,
        } => {
            updater_command(action, expand(&data_dir), config.map(|c| expand(&c))).await;
        }
        Command::Enterprise {
            action,
            data_dir,
            config,
        } => {
            enterprise_command(action, expand(&data_dir), config.map(|c| expand(&c))).await;
        }
        Command::Commerce {
            action,
            data_dir,
            config,
        } => {
            commerce_command(action, expand(&data_dir), config.map(|c| expand(&c))).await;
        }
        Command::Worker {
            action,
            data_dir,
            config,
        } => {
            let data_dir = expand(&data_dir);
            let config = config.map(|c| expand(&c));
            match action {
                WorkerAction::Run { iterations } => {
                    if let Err(e) = worker_node::run(iterations, data_dir, config).await {
                        eprintln!("worker node: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Sessions { data_dir } => {
            sessions(expand(&data_dir)).await;
        }
        Command::Bootstrap {
            install_root,
            exec_args,
        } => {
            bootstrap::run_launcher(
                expand(&install_root),
                exec_args
                    .into_iter()
                    .map(std::ffi::OsString::from)
                    .collect(),
            );
        }
        Command::Build => {
            println!(
                "{}",
                serde_json::to_string_pretty(&build_report_json(true))
                    .unwrap_or_else(|_| build_report_json(true).to_string())
            );
        }
    }
}

/// The running-build report: version, executable path, optionally the
/// sha256 of the RUNNING executable, and the release identity the bootstrap
/// launcher verified (absent when the binary was started directly). The
/// daemon logs the same document on startup (without the self-hash: the
/// launcher already verified those bytes, and daemon startup must not do a
/// full-binary hash pass) and folds the digest into the health `version`, so
/// a supervisor can observe WHICH artifact is live.
fn build_report_json(include_self_digest: bool) -> Value {
    let self_sha256 = if include_self_digest {
        faktor_updater::self_digest().ok()
    } else {
        None
    };
    let release = faktor_updater::running_release();
    let (release_id, release_digest) = match &release {
        Some((id, digest)) => (Some(id.clone()), Some(digest.clone())),
        None => (None, None),
    };
    let matches_release = match (&self_sha256, &release_digest) {
        (Some(actual), Some(expected)) => Some(actual == expected),
        _ => None,
    };
    json!({
        "schema": "faktor-build-report/v1",
        "version": faktor_core::VERSION,
        "exe": std::env::current_exe()
            .ok()
            .map(|p| p.display().to_string()),
        "self_sha256": self_sha256,
        "release_id": release_id,
        "release_digest": release_digest,
        "self_digest_matches_release": matches_release,
        "install_root": faktor_updater::running_install_root()
            .map(|p| p.display().to_string()),
    })
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
    let config = config.unwrap_or_default();
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    // 1-2: the durable store + CAS (full integrity scan on this entry).
    let session = SessionManager::open(data_dir.join("store"), data_dir.join("cas"), true)
        .map_err(|e| e.to_string())?;
    // 3: ONE daemon supervisor (audit P0-40): the SAME Arc supervises every
    // MCP server child, every hook child, every tool/terminal child.
    let supervisor = ProcessSupervisor::new(session.cas());
    build_daemon_core(
        data_dir,
        session,
        supervisor,
        config,
        vec![],
        None,
        graph::SemanticCfg::default(),
    )
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
async fn build_daemon_with_mcp_and_chunks_fast(
    data_dir: &std::path::Path,
    config: Option<config::Config>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
    semantic: graph::SemanticCfg,
) -> Result<DaemonGraph, String> {
    build_daemon_with_mcp_inner(data_dir, config, chunk_tx, true, semantic).await
}

async fn build_daemon_with_mcp_inner(
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
    // supervisor above).
    let mut graph = build_daemon_core(
        data_dir, session, supervisor, config, mcp_tools, chunk_tx, semantic,
    )?;
    graph.mcp_servers = servers;
    Ok(graph)
}

/// Maximum FAKTOR_HOOKS entries honored (bounding the env surface).
const MAX_ENV_HOOKS: usize = 8;

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
const DAEMON_HOOK_ENVELOPE: CapabilitySet = CapabilitySet::ALL;

/// Build an optional lifecycle-hook registry over the DAEMON supervisor
/// (audit P0-40). Each parsed spec is logged; a spec the registry rejects
/// is a loud warning, never a daemon failure.
fn env_hook_registry(
    supervisor: &Arc<ProcessSupervisor>,
) -> Option<Arc<faktor_hooks::HookRegistry>> {
    let specs = parse_hooks_env(&std::env::var("FAKTOR_HOOKS").unwrap_or_default());
    hook_registry(supervisor, specs)
}

/// Shared hook-registry construction (test seam + env path): the registry
/// is rooted at the GIVEN supervisor and granted the daemon envelope, so
/// every hook child lands in the daemon's single bounded registry.
fn hook_registry(
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
struct SessionWorkspaceRoots(Arc<SessionManager>);

impl SessionWorkspaceRoots {
    fn resolve(&self, workspace_id: u64) -> Option<PathBuf> {
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
fn daemon_instructions_resolver(
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
fn daemon_outbound_scan(config: &config::Config) -> OutboundScanConfig {
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
fn register_commerce_credential(
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
fn daemon_egress_transport(
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
fn daemon_verification(
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
fn efficiency_flags(cfg: &config::EfficiencyCfg) -> faktor_agent::EfficiencyFlags {
    faktor_agent::EfficiencyFlags {
        failure_learning: cfg.failure_learning,
        ccr: cfg.ccr,
        typed_handoff: cfg.typed_handoff,
        semantic_context: cfg.semantic_context,
        rework_routing: cfg.rework_routing,
    }
}

use faktor_learning::LearningStore as _;

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
struct LearningRiskPrior {
    session: Arc<SessionManager>,
    cache: std::sync::Mutex<RiskCache>,
}

/// The cached merged omission-risk index plus the durable stamp it was built
/// from: `(session id, newest ledger seq)` sorted by session id. The stamp
/// changes exactly when a session's typed ledger advances — the only way a
/// learning corpus can grow.
#[derive(Debug, Default)]
struct RiskCache {
    stamp: Vec<(u64, i64)>,
    risks: std::collections::HashMap<faktor_core::FileHash, f64>,
}

impl LearningRiskPrior {
    /// Build the adapter over the daemon's session manager, snapshotting
    /// every durable corpus ONCE (reopen-safe). An empty corpus yields an
    /// empty index, so every lookup is neutral (`1.0`) and context selection
    /// stays byte-identical to the flag-off path.
    fn from_session(session: Arc<SessionManager>) -> Self {
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
    fn stamp(session: &Arc<SessionManager>) -> Vec<(u64, i64)> {
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
    fn merged_risks(
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
    fn cache(&self) -> std::sync::MutexGuard<'_, RiskCache> {
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
fn daemon_context_prior(
    enabled: bool,
    session: &Arc<SessionManager>,
) -> Option<Arc<dyn faktor_context::information::FailurePrior + Send + Sync>> {
    if !enabled {
        return None;
    }
    Some(Arc::new(LearningRiskPrior::from_session(session.clone())))
}

/// The graph construction core (audit 12/17): steps 4-16 of
/// [`graph::DAEMON_CONSTRUCTION_ORDER`] are built HERE, inline and in the
/// documented order (the ordering test scans this function's body).
/// Callers open the store and create the ONE supervisor (steps 1-3) — the
/// async MCP connect must ride that same supervisor — and everything else
/// of the daemon lifetime is this function's construction.
fn build_daemon_core(
    data_dir: &std::path::Path,
    session: Arc<SessionManager>,
    supervisor: Arc<ProcessSupervisor>,
    config: config::Config,
    extra_tools: Vec<faktor_agent::Tool>,
    chunk_tx: Option<std::sync::Arc<faktor_agent::ChunkSink>>,
    semantic: graph::SemanticCfg,
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
            let dyn_arc: Arc<dyn Provider> = ollama.clone();
            providers
                .try_register(dyn_arc)
                .map_err(|e| format!("provider {} failed to register: {e}", p.id()))?;
            ollama_warmers.push(ollama);
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
    let commerce = tools_market::open_commerce_service_with(
        data_dir,
        &config.commerce,
        Arc::new(tools_market::CasArtifacts::with_secrets(
            cas.clone(),
            commerce_secrets.clone(),
        )),
        commerce_seams,
    )?;
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
        agent,
        evidence,
        orchestrator,
        shadows,
        tasks,
    })
}

/// Automatic-backup interval (audit 44): at most one snapshot per
/// `BACKUP_MIN_INTERVAL_SECS` of wall time — unless the newest backup no
/// longer matches the store's size, which means the store changed since the
/// snapshot was taken (a crash-recovery run counts).
const BACKUP_MIN_INTERVAL_SECS: u64 = 3600;
/// Retention quota (spec §24): keep at most this many complete backups…
const BACKUP_MAX_FILES: usize = 8;
/// …and at most this many bytes across the whole backups directory
/// (drop the oldest while either bound is exceeded).
const BACKUP_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Post-readiness delay before the startup backup task acts: the daemon is
/// announced and accepting connections well before any snapshot work starts.
const BACKUP_START_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

/// Every COMPLETE backup under `<data_dir>/backups` (`faktor-plus-*.db`),
/// newest by mtime first. In-progress snapshots write under a `.db.tmp-*`
/// name and are published into place only when complete (one atomic
/// `faktor_fs::atomic::atomic_adopt`), so they are invisible here by
/// construction.
fn list_backups(data_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let backups = data_dir.join("backups");
    let Ok(files) = std::fs::read_dir(&backups) else {
        return Vec::new();
    };
    let mut out: Vec<std::path::PathBuf> = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with("faktor-plus-") && name.ends_with(".db")
        })
        .collect();
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    out.reverse();
    out
}

/// Interval + staleness gate: the startup backup is due when no backup
/// exists, when the newest is older than [`BACKUP_MIN_INTERVAL_SECS`], or
/// when the newest no longer matches the store file's size (the daemon
/// wrote since it was taken).
fn backup_due(data_dir: &std::path::Path) -> bool {
    let db_path = data_dir.join("store").join("faktor-plus.db");
    let Ok(db_meta) = std::fs::metadata(&db_path) else {
        return false;
    };
    let Some(newest) = list_backups(data_dir).into_iter().next() else {
        return true;
    };
    let meta = std::fs::metadata(&newest).ok();
    let stale = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(|m| {
            m.elapsed()
                .map(|e| e >= std::time::Duration::from_secs(BACKUP_MIN_INTERVAL_SECS))
                .unwrap_or(true)
        })
        .unwrap_or(true);
    let resized = meta.map(|m| m.len()).unwrap_or(0) != db_meta.len();
    stale || resized
}

/// Remove interrupted-backup temp files older than an hour (a crashed writer
/// can leave them behind; live writers are always younger). Best effort.
fn sweep_stale_backup_tmp(backups: &std::path::Path) {
    let Ok(files) = std::fs::read_dir(backups) else {
        return;
    };
    for f in files.flatten() {
        let p = f.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.contains(".db.tmp-") || !older_than(&p, std::time::Duration::from_secs(3600)) {
            continue;
        }
        let _ = std::fs::remove_file(&p);
    }
}

/// True when the file's mtime is at least `age` in the past (missing or
/// unreadable files are never "stale": fail closed).
fn older_than(p: &std::path::Path, age: std::time::Duration) -> bool {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|e| e > age)
        .unwrap_or(false)
}

/// Online backup with rotation (spec §24): one crash-safe snapshot per
/// daemon start the interval gate admits; retention keeps the newest
/// [`BACKUP_MAX_FILES`] and never more than [`BACKUP_MAX_TOTAL_BYTES`] total.
/// The snapshot is written to a `.db.tmp-*` name and published as ONE
/// atomic step through `faktor_fs::atomic::atomic_adopt` (fsync the temp,
/// rename into place, fsync the directory), so a crash mid-backup can never
/// leave a partial file that reads as a complete backup (and the
/// gate/retention scans never see one). Best effort — a backup failure
/// never stops the daemon.
fn rotate_backup(store: &faktor_store::Store, data_dir: &std::path::Path) {
    let backups = data_dir.join("backups");
    if std::fs::create_dir_all(&backups).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = backups.join(format!("faktor-plus-{ts}.db"));
    let tmp = backups.join(format!("faktor-plus-{ts}.db.tmp-{}", std::process::id()));
    if let Err(e) = store.backup_to(&tmp) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("automatic backup failed: {e}");
        return;
    }
    if let Err(e) = faktor_fs::atomic::atomic_adopt(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("automatic backup finalize failed: {e}");
        return;
    }
    tracing::info!("automatic backup written to {}", dest.display());
    // Retention quota: drop the OLDEST files while the count exceeds
    // BACKUP_MAX_FILES or the total bytes exceed BACKUP_MAX_TOTAL_BYTES.
    // The just-written snapshot is newest and never a candidate.
    let files = list_backups(data_dir);
    let mut total: u64 = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut kept = files.len();
    for victim in files.iter().rev() {
        let over_count = kept > BACKUP_MAX_FILES;
        let over_bytes = total > BACKUP_MAX_TOTAL_BYTES && kept > 1;
        if !over_count && !over_bytes {
            break;
        }
        if let Ok(m) = std::fs::metadata(victim) {
            total = total.saturating_sub(m.len());
        }
        if std::fs::remove_file(victim).is_err() {
            break;
        }
        kept -= 1;
    }
    // Opportunistic sweep of interrupted-writer debris from crashed runs.
    sweep_stale_backup_tmp(&backups);
}

/// Startup-backup task seam (P0-46): the async wrapper sleeps the
/// post-readiness delay, applies the interval/staleness gate, and runs the
/// SYNC snapshot+rotation on the blocking pool — never on a Tokio worker.
/// Returns the JoinHandle the shutdown path drains.
fn spawn_startup_backup(
    store: Arc<faktor_store::Store>,
    data_dir: std::path::PathBuf,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn(async move {
        tokio::time::sleep(BACKUP_START_DELAY).await;
        if backup_due(&data_dir) {
            let store = store.clone();
            let dir = data_dir.clone();
            // spawn_blocking: the SQLite backup API is synchronous and can
            // hold the caller's thread for the whole snapshot; a Tokio
            // worker must never sit in it (P0-46 worker starvation).
            if let Err(e) = tokio::task::spawn_blocking(move || rotate_backup(&store, &dir)).await {
                tracing::warn!("startup backup worker failed: {e}");
            }
        } else {
            tracing::info!(
                "startup backup skipped: a backup newer than {BACKUP_MIN_INTERVAL_SECS}s exists"
            );
        }
    })
}

/// Bounded graceful window of the daemon's detached orchestrated drives at
/// shutdown. The registry then aborts and reaps stragglers within its own
/// [`faktor_orchestrator::runtime::task_executor::TaskDriveRegistry::ABORT_REAP_GRACE`],
/// so the WHOLE drive drain stays bounded by `grace + ABORT_REAP_GRACE` —
/// never unbounded, and never a dropped durable write (an aborted drive is
/// crash-equivalent and resumable from its durable rows).
const SERVE_DRIVE_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// The ONE post-signal daemon shutdown sequence (serve):
///
/// 0. stop the native listener (when its handle is passed): the handle OWNS
///    the serve task, so this is the bounded graceful join
///    ([`faktor_server::api::ServerHandle::shutdown`]) — never a detached
///    future; an unexpected death is logged with its typed status BEFORE the
///    stop, so it is never masked as a requested one;
/// 1. stop the worker plane (when enabled): the slot hands back the owned
///    `WorkerPlaneHandle`, which requests graceful shutdown and JOINS its
///    serve task within
///    [`faktor_server::worker_plane::WORKER_PLANE_SHUTDOWN_BOUND`] (a
///    straggler is aborted), so no new remote worker request lands while
///    the drives drain and no listener task is ever detached. An unexpected
///    death is logged with its typed code (the same status health and any
///    placement decision query) and the slot records the typed terminal
///    state;
/// 2. stop the graph-hosted repository index reconciliation worker (when
///    hosted): `IndexService::shutdown_worker` cancels and JOINS the owned
///    worker within its own bound (a straggler is aborted and awaited), so
///    no background index pass outlives the daemon or races the drain;
/// 3. stop the runtime's OWN lazily-hosted repository index worker (when the
///    runtime ever opened one): `AgentRuntime::shutdown_index_service`
///    cancels and JOINS the owned worker within the same service bound and
///    reports the outcome typed. A runtime that never hosted one is an inert
///    no-op — the accessor never opens the service as a side effect. This is
///    non-fatal by contract: a typed abort/join failure is logged, never
///    propagated (the durable rows stay the recovery authority);
/// 4. abort the post-ready loops (verification executor, SCM re-sync, billing
///    report schedule) — none of them is a durable-write producer whose
///    in-flight work is lost, and each re-runs its durable claims at the next
///    boot;
/// 5. close the TaskExecutor's drive registry and bounded-drain every
///    detached drive (the registry's graceful-then-abort contract: give
///    in-flight drives `grace` to land their record-first durable
///    writes/settlement, then abort and reap the stragglers within
///    `ABORT_REAP_GRACE`; an aborted run stays resumable through
///    `TaskExecutor::resume_run`). This MUST happen while the store and the
///    shadow service are still alive, so it runs here — before the backup
///    drain and before `graph` drops;
/// 6. drain the startup-backup task and the owned Ollama warm-up threads
///    (bounded), AFTER the drives so the final snapshot observes every
///    settled durable write.
///
/// The returned [`DriveDrainReport`](faktor_orchestrator::runtime::task_executor::DriveDrainReport)
/// is the testable evidence of the drive drain.
// Eight explicit owners/tasks is the sequence's whole shape; bundling them
// into a struct would not make the shutdown contract clearer.
#[allow(clippy::too_many_arguments)]
async fn shutdown_serving_daemon(
    tasks: &Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
    server: Option<faktor_server::api::ServerHandle>,
    worker_plane: Option<faktor_server::api::WorkerPlaneListener>,
    index: Option<Arc<faktor_index::IndexService>>,
    agent: Option<&Arc<AgentRuntime>>,
    verification_executor: tokio::task::JoinHandle<()>,
    scm_task: Option<tokio::task::JoinHandle<()>>,
    billing_report_task: Option<tokio::task::JoinHandle<()>>,
    backup_task: tokio::task::JoinHandle<()>,
) -> faktor_orchestrator::runtime::task_executor::DriveDrainReport {
    // Stop the native listener first (the primary intake): the handle OWNS
    // the serve task, so this is a bounded join, never a detached future.
    // Health is queried BEFORE the request so an unexpected death is named,
    // not masked by the stop (the same contract the worker plane follows
    // below).
    if let Some(server) = server {
        let health = server.status();
        if health.is_unavailable() {
            tracing::error!("{}", health.health_line());
        }
        match server.shutdown().await {
            Ok(()) => tracing::info!("native server: stopped (bounded graceful join)"),
            Err(error) => tracing::error!(
                code = error.code(),
                "native server: shutdown failed: {error}"
            ),
        }
    }
    // Stop the remote intake first: the slot owns the serve task, so this
    // is a bounded join, never a detached listener. Health is queried BEFORE
    // the request so an unexpected death is named, not masked by the stop;
    // the slot then records the typed terminal state for any later health
    // read.
    if let Some(listener) = worker_plane {
        let taken = listener
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_for_shutdown();
        if let Some((handle, health)) = taken {
            if health.is_unavailable() {
                tracing::error!("{}", health.health_line());
            }
            let terminal = match handle.shutdown().await {
                Ok(()) => {
                    tracing::info!("worker plane: stopped (bounded graceful join)");
                    faktor_server::worker_plane::WorkerPlaneStatus::Stopped
                }
                Err(error) => {
                    tracing::error!(
                        code = error.code(),
                        "worker plane: shutdown failed: {error}"
                    );
                    faktor_server::worker_plane::WorkerPlaneStatus::Unavailable {
                        code: error.code(),
                        message: error.to_string(),
                    }
                }
            };
            listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .record_terminal(terminal);
        }
    }
    // Stop the repository index reconciliation worker (bounded join, before
    // the drive drain): a background pass must not outlive the daemon or
    // race the shutdown. `NotRunning` is the honest no-op when the worker
    // was never started (or already stopped).
    if let Some(index) = index {
        let started = std::time::Instant::now();
        match index.shutdown_worker().await {
            faktor_index::WorkerShutdown::NotRunning => {}
            faktor_index::WorkerShutdown::Joined => tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "index reconciliation worker: stopped (bounded graceful join)"
            ),
            faktor_index::WorkerShutdown::Aborted => {
                // F8: the async task was aborted, but a blocking pass cannot
                // be killed — report the TYPED residual so the operator knows
                // the real exit bound is the pass's remaining duration.
                let pass_in_flight = index.worker_status().pass_in_flight;
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    pass_in_flight,
                    "index reconciliation worker: did not stop within the bound; aborted and reaped{}",
                    if pass_in_flight {
                        " — a blocking pass is STILL RUNNING and sets the real exit bound"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    // Stop the runtime's OWN lazily-hosted index worker (adjacent to the
    // graph-hosted one, before the drive drain): the runtime has no async
    // teardown, so this accessor IS its join point. `None` = the runtime
    // never hosted a service (never opened); the call never opens one.
    // Non-fatal: an abort/join failure is logged typed, the shutdown
    // continues, and the durable rows remain the recovery authority.
    if let Some(agent) = agent {
        let started = std::time::Instant::now();
        match agent.shutdown_index_service().await {
            None => {}
            Some(faktor_index::WorkerShutdown::NotRunning) => {}
            Some(faktor_index::WorkerShutdown::Joined) => tracing::info!(
                elapsed_ms = started.elapsed().as_millis() as u64,
                "runtime index worker: stopped (bounded graceful join)"
            ),
            Some(faktor_index::WorkerShutdown::Aborted) => {
                let pass_in_flight = agent
                    .index_service_worker_status()
                    .is_some_and(|status| status.pass_in_flight);
                tracing::error!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    pass_in_flight,
                    "runtime index worker: did not stop within the bound; aborted and reaped{}",
                    if pass_in_flight {
                        " — a blocking pass is STILL RUNNING and sets the real exit bound"
                    } else {
                        ""
                    }
                );
            }
        }
    }
    verification_executor.abort();
    if let Some(task) = scm_task {
        task.abort();
    }
    if let Some(task) = billing_report_task {
        task.abort();
    }
    let started = std::time::Instant::now();
    let report = tasks.shutdown_drives(SERVE_DRIVE_SHUTDOWN_GRACE).await;
    if report.total > 0 || report.unreaped > 0 {
        tracing::info!(
            total = report.total,
            completed = report.completed,
            aborted = report.aborted,
            unreaped = report.unreaped,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "orchestrated drive shutdown drain"
        );
    }
    if report.unreaped > 0 {
        tracing::error!(
            unreaped = report.unreaped,
            "drive shutdown could not reap every aborted drive; the durable rows remain the recovery authority"
        );
    }
    drain_startup_backup(backup_task).await;
    report
}

/// Shutdown drain for the startup-backup task: waits a bounded window for
/// the backup to finish (the daemon announced readiness long ago, so the
/// snapshot is normally long done), then aborts the async wrapper. The
/// abort cuts the sleep/gate, NOT the blocking snapshot (spawn_blocking
/// closures cannot be force-killed; the snapshot is bounded and finishes at
/// its own pace) — the daemon never waits unboundedly on shutdown.
async fn drain_startup_backup(mut backup_task: tokio::task::JoinHandle<()>) {
    const BACKUP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
    // `&mut JoinHandle` is a Future: the handle stays borrowable so the
    // timeout path can still abort the async wrapper.
    match tokio::time::timeout(BACKUP_DRAIN_GRACE, &mut backup_task).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("startup backup task panicked: {e}"),
        Err(_) => {
            tracing::warn!("startup backup drain timed out; aborting the async wrapper");
            backup_task.abort();
        }
    }
    // The SAME bounded shutdown drain retires the owned Ollama warm-up
    // threads (audited spawn ownership): bounded join, deterministic runtime
    // stop inside each thread, never a leaked handle or unbounded wait.
    let (joined, detached) = drain_ollama_warmups(OLLAMA_WARMUP_DRAIN_GRACE);
    if joined + detached > 0 {
        tracing::info!(joined, detached, "ollama warm-up shutdown drain");
    }
}

/// The process-wide owner of every detached Ollama warm-up thread.
///
/// Audited gap: `warm_ollama` used to `std::thread::spawn` a thread that
/// owned a private Tokio runtime and DROP its `JoinHandle` — the thread (and
/// its runtime) leaked until process exit and no shutdown path could bound
/// it. Each warm-up is now an [`OllamaWarmup`] owner held in this registry:
/// the thread signals completion through a channel, stops its runtime
/// deterministically (`Runtime::shutdown_timeout`) before it exits, and the
/// shutdown drain joins it with a bounded timeout.
static OLLAMA_WARMUPS: std::sync::OnceLock<std::sync::Mutex<Vec<OllamaWarmup>>> =
    std::sync::OnceLock::new();

/// Bounded stop of the warm-up's private runtime after the probe returned:
/// a probe task that somehow lingers cannot outlive its owner's thread (the
/// blocking runtime shutdown cancels what it can and joins the blocking
/// pool within the grace).
const OLLAMA_WARMUP_RUNTIME_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
/// Bounded shutdown join window of the warm-up THREADS (never unbounded).
const OLLAMA_WARMUP_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// One owned Ollama warm-up thread: the retained `JoinHandle` plus the
/// completion signal of its body. `join_with_timeout` is the ONLY way to
/// retire it — a dropped owner would recreate the old leak.
struct OllamaWarmup {
    handle: Option<std::thread::JoinHandle<()>>,
    finished: Option<std::sync::mpsc::Receiver<()>>,
}

impl OllamaWarmup {
    /// Bounded join: `true` when the body finished (and its runtime was
    /// stopped) within `timeout`. A panicked body is joined too (its
    /// completion signal is disconnected, not missing). On timeout the
    /// thread is left detached — a thread cannot be force-killed — but it is
    /// a best-effort probe whose own runtime shutdown is already bounded,
    /// and the daemon never waits unboundedly on shutdown.
    fn join_with_timeout(self, timeout: std::time::Duration) -> bool {
        let (Some(handle), Some(finished)) = (self.handle, self.finished) else {
            return true;
        };
        match finished.recv_timeout(timeout) {
            Ok(()) => {
                let _ = handle.join();
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if handle.join().is_err() {
                    tracing::warn!("ollama warm-up thread panicked (joined)");
                }
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
        }
    }
}

/// Spawn one named warm-up thread whose body signals completion on exit
/// (including panic: the wrapper's sender is owned by the thread stack).
/// `None` when the OS refused the thread (the warm-up is skipped, never a
/// half-owned handle).
fn spawn_ollama_warmup_thread(
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> Option<OllamaWarmup> {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    match std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            body();
            let _ = done_tx.send(());
        }) {
        Ok(handle) => Some(OllamaWarmup {
            handle: Some(handle),
            finished: Some(done_rx),
        }),
        Err(e) => {
            tracing::warn!("ollama warm-up thread failed to spawn: {e}");
            None
        }
    }
}

/// Register one owned warm-up in the process registry (the SAME path
/// `warm_ollama` uses; tests register seam bodies through it).
fn register_ollama_warmup(owner: OllamaWarmup) {
    let registry = OLLAMA_WARMUPS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(owner);
}

/// Bounded join of an explicit owner set: `(joined, detached)` counts. A
/// detached owner is logged (its own runtime stop is already bounded).
fn drain_warmup_owners(owners: Vec<OllamaWarmup>, timeout: std::time::Duration) -> (usize, usize) {
    let deadline = std::time::Instant::now() + timeout;
    let mut joined = 0usize;
    let mut detached = 0usize;
    for owner in owners {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if owner.join_with_timeout(remaining) {
            joined += 1;
        } else {
            detached += 1;
            tracing::warn!(
                "ollama warm-up thread missed the shutdown join window; it stays detached and exits with its own bounded runtime stop"
            );
        }
    }
    (joined, detached)
}

/// Shutdown drain of every registered warm-up: takes the registry (idempotent
/// — a later call finds none) and joins each owner within `timeout` in total.
/// The daemon's shutdown path reaches this through `drain_startup_backup`,
/// the one bounded post-ready task drain.
fn drain_ollama_warmups(timeout: std::time::Duration) -> (usize, usize) {
    let owners: Vec<OllamaWarmup> = match OLLAMA_WARMUPS.get() {
        Some(registry) => std::mem::take(
            &mut *registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        ),
        None => Vec::new(),
    };
    drain_warmup_owners(owners, timeout)
}

/// Live capability warm-up for one Ollama provider (spec §10): the
/// concrete Arc is owned by the spawned thread, so probing reaches the
/// SAME instance the registry serves. Best-effort, never blocks; the thread
/// is OWNED by the process registry and bounded-joined at shutdown
/// ([`drain_ollama_warmups`]).
fn warm_ollama(ollama: Arc<faktor_ollama::OllamaProvider>) {
    let Some(owner) = spawn_ollama_warmup_thread("faktor-ollama-warmup", move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::warn!("ollama warm-up runtime failed: {e}");
                return;
            }
        };
        match rt.block_on(ollama.refresh_from_live()) {
            Ok(n) => {
                tracing::info!("ollama: probed {n} model(s) from live discovery");
            }
            Err(e) => {
                tracing::warn!("ollama warm-up failed (defaults stay): {e}");
            }
        }
        // Deterministic runtime stop: no probe task outlives its thread.
        rt.shutdown_timeout(OLLAMA_WARMUP_RUNTIME_STOP_GRACE);
    }) else {
        return;
    };
    register_ollama_warmup(owner);
}

/// Adversarial covers of the audited Ollama warm-up ownership gap: the
/// handle is retained, the bounded join is honored, and the shutdown drain
/// empties the process registry (idempotently) instead of leaking threads.
#[cfg(test)]
mod ollama_warmup_tests {
    use super::*;

    /// The global registry is process-wide: tests that touch it serialize.
    static GLOBAL_WARMUP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn bounded_join_reports_finished_owners_and_detaches_stragglers() {
        let fast = spawn_ollama_warmup_thread("faktor-test-warmup-fast", || {}).unwrap();
        assert!(fast.join_with_timeout(std::time::Duration::from_secs(5)));
        // A panicked body is finished (its sender disconnected), not a
        // straggler: the owner is joined, never reported as detached.
        let panicked = spawn_ollama_warmup_thread("faktor-test-warmup-panic", || {
            panic!("warm-up body panic (test)");
        })
        .unwrap();
        assert!(panicked.join_with_timeout(std::time::Duration::from_secs(5)));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let slow = spawn_ollama_warmup_thread("faktor-test-warmup-slow", move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        let started = std::time::Instant::now();
        assert!(!slow.join_with_timeout(std::time::Duration::from_millis(25)));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "bounded join never waits unboundedly"
        );
        // Let the detached thread exit so the test binary holds no runner.
        let _ = release_tx.send(());
    }

    #[test]
    fn shutdown_drain_empties_the_process_registry_within_bound() {
        let _serial = GLOBAL_WARMUP_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        register_ollama_warmup(
            spawn_ollama_warmup_thread("faktor-test-warmup-registered", || {}).unwrap(),
        );
        let started = std::time::Instant::now();
        let (joined, detached) = drain_ollama_warmups(std::time::Duration::from_secs(5));
        assert_eq!(joined, 1);
        assert_eq!(detached, 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // Idempotent: the registry was TAKEN, nothing is left to leak.
        let (joined, detached) = drain_ollama_warmups(std::time::Duration::from_millis(1));
        assert_eq!((joined, detached), (0, 0));
    }

    #[test]
    fn shutdown_drain_never_exceeds_the_bound_for_a_parked_thread() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let parked = spawn_ollama_warmup_thread("faktor-test-warmup-parked", move || {
            let _ = release_rx.recv();
        })
        .unwrap();
        let started = std::time::Instant::now();
        let (joined, detached) =
            drain_warmup_owners(vec![parked], std::time::Duration::from_millis(30));
        assert_eq!((joined, detached), (0, 1));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the drain stayed bounded"
        );
        let _ = release_tx.send(());
    }
}

/// Config resolution for `serve` (audit 31): an EXPLICIT --config path must
/// load STRICTLY (parse + semantic validation) — any failure is an Err the
/// caller turns into a startup error (exit 1); the daemon never boots on a
/// config it cannot fully honor. Without --config, defaults + best-effort
/// discovery stay lenient and nothing here can fail startup.
/// Test-facing convenience over [`serve_config_and_semantic`].
#[cfg(test)]
fn serve_config(config_path: Option<PathBuf>) -> Result<config::Config, String> {
    Ok(serve_config_and_semantic(config_path)?.0)
}

/// The strict daemon config PLUS the additive `[semantic]` section (audits
/// 48-54/58/79). The section is extracted from the raw document BEFORE the
/// frozen `Config` shape parses the rest, so it is strictly additive with an
/// empty default: absent/null keeps [`graph::SemanticCfg::default`] and a
/// present section is parsed with `deny_unknown_fields` (a typo'd key fails
/// startup, never silently changes behavior). Every other key keeps exactly
/// the strict load semantics (`Config` parse + `validate`).
fn serve_config_and_semantic(
    config_path: Option<PathBuf>,
) -> Result<(config::Config, graph::SemanticCfg), String> {
    let Some(path) = config_path else {
        return Ok((config::Config::default(), graph::SemanticCfg::default()));
    };
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("config {}: {e}", path.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("config {}: {e}", path.display()))?;
    let semantic = match value
        .as_object_mut()
        .and_then(|object| object.remove("semantic"))
    {
        None | Some(serde_json::Value::Null) => graph::SemanticCfg::default(),
        Some(section) => serde_json::from_value(section)
            .map_err(|e| format!("config {} [semantic]: {e}", path.display()))?,
    };
    let config: config::Config =
        serde_json::from_value(value).map_err(|e| format!("config {}: {e}", path.display()))?;
    config
        .validate()
        .map_err(|e| format!("config {}: {e}", path.display()))?;
    // Strict provider-section validation (bounded caps, unique ids, valid
    // command/args/endpoint/timeout/auth-env for every external provider):
    // a hostile section refuses startup here, before any graph authority or
    // child process exists.
    semantic
        .validate()
        .map_err(|e| format!("config {} [semantic]: {e}", path.display()))?;
    Ok((config, semantic))
}

/// Forced-exit code of the SECOND shutdown signal during the drain (`128 +
/// SIGINT`, the conventional interrupted exit; SIGTERM maps to the same
/// bounded force-exit since the daemon's own drain is the graceful path).
const FORCE_EXIT_CODE: i32 = 130;

/// Test-only probe of the force-exit seam: a test cannot let the harness
/// process exit, so an installed probe receives the code instead. Production
/// has no probe installed and `std::process::exit` runs.
#[cfg(test)]
static FORCE_EXIT_PROBE: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<i32>>> =
    std::sync::Mutex::new(None);

/// Test-only marker: the FIRST shutdown signal was observed and the drain
/// (plus the second-signal watchdog) is being entered. Lets the signal tests
/// synchronize deterministically instead of sleeping.
#[cfg(test)]
static SIGNAL_DRAIN_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only count of armed signal waiters (handler registrations that
/// succeeded). A test delivers a real signal only once the waiter it targets
/// is armed, so the process can never be killed by an unhandled default.
#[cfg(test)]
static SIGNAL_WAITERS_ARMED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Await the next process shutdown signal (SIGTERM or SIGINT). Registration
/// is process-wide; a registration failure is loud and falls back to
/// `ctrl_c`, never a panic. The returned label is the signal's name for
/// structured logs.
async fn wait_for_shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                #[cfg(test)]
                SIGNAL_WAITERS_ARMED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::select! {
                    _ = term.recv() => "SIGTERM",
                    _ = int.recv() => "SIGINT",
                }
            }
            (term, int) => {
                let err = term.err().or(int.err());
                tracing::error!(
                    error = %err.map(|e| e.to_string()).unwrap_or_default(),
                    "shutdown signal handlers could not be installed; falling back to ctrl_c"
                );
                let _ = tokio::signal::ctrl_c().await;
                "ctrl_c"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "ctrl_c"
    }
}

/// The second-signal force path: the drain is bounded, but an operator who
/// sends a second SIGTERM/SIGINT asks to stop NOW. The code is reported to
/// the test probe when one is installed; production exits immediately.
async fn on_force_signal(signal: &str) {
    tracing::error!(
        signal,
        code = FORCE_EXIT_CODE,
        "second shutdown signal during the drain; forcing exit"
    );
    #[cfg(test)]
    if let Some(probe) = FORCE_EXIT_PROBE.lock().unwrap().take() {
        let _ = probe.send(FORCE_EXIT_CODE);
        return;
    }
    std::process::exit(FORCE_EXIT_CODE);
}

/// Arm the second-signal watchdog. Owned by the daemon's shutdown path: it
/// lives only while the bounded drain runs and either fires (force exit) or
/// is dropped when the process ends normally.
fn spawn_force_exit_watchdog() {
    tokio::spawn(async move {
        let signal = wait_for_shutdown_signal().await;
        on_force_signal(signal).await;
    });
}

async fn serve(port: u16, data_dir: PathBuf, config_path: Option<PathBuf>) {
    if let Err(e) = serve_impl(port, data_dir, config_path, None, None).await {
        tracing::error!("{e}");
        std::process::exit(1);
    }
}

/// Typed summary of the startup verification recovery sweep. Every count is
/// diagnostic and asserted by tests; a read failure is NEVER silently a
/// no-op — `unreadable`/`scan_failed` name the durable work left for the
/// next boot.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct VerificationRecoverySummary {
    requeued: usize,
    orphaned: usize,
    touched: usize,
    /// Sessions whose durable verification rows could not be read (store
    /// error, or a listed session whose handle vanished): their rows are
    /// LEFT AS THEY ARE (durable work) and the failure is logged typed.
    unreadable: usize,
    /// The session scan itself failed: nothing could be swept this boot.
    scan_failed: bool,
}

/// Daemon startup verification recovery sweep (audit P0-5/26 production
/// wiring): every session's stale `Running` verification jobs are re-queued
/// before the executor starts, so a check whose executor died mid-run is
/// retried honestly — never silently dropped, never a pass. Runs BEFORE
/// readiness is announced, like every other crash-recovery step. A store
/// error is loud and leaves the durable rows for the next boot; it is never
/// reported as "nothing to recover".
fn recover_verification_jobs_at_startup(
    session: &Arc<faktor_session::SessionManager>,
) -> VerificationRecoverySummary {
    let mut summary = VerificationRecoverySummary::default();
    let ids = match session.store().session_ids() {
        Ok(ids) => ids,
        Err(e) => {
            summary.scan_failed = true;
            tracing::error!(
                error = %e,
                "verification recovery could not scan sessions; every durable verification row \
                 stays for the next boot: {e}"
            );
            return summary;
        }
    };
    for sid in ids {
        match session.get_session(sid) {
            Ok(Some(handle)) => match handle.recover_verification_jobs_after_restart() {
                Ok(report) if report.requeued + report.orphaned > 0 => {
                    summary.requeued += report.requeued;
                    summary.orphaned += report.orphaned;
                    summary.touched += 1;
                }
                Ok(_) => {}
                Err(e) => {
                    summary.unreadable += 1;
                    tracing::error!(
                        session = %sid,
                        error = %e,
                        "verification recovery for session {sid} could not read its durable rows; \
                         they stay for the next boot: {e}"
                    );
                }
            },
            Ok(None) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %sid,
                    "verification recovery found a listed session with no handle (store \
                     inconsistency); its durable rows could not be swept this boot"
                );
            }
            Err(e) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %sid,
                    error = %e,
                    "verification recovery could not open session {sid}; its durable rows stay \
                     for the next boot: {e}"
                );
            }
        }
    }
    if summary.requeued + summary.orphaned > 0 || summary.unreadable > 0 || summary.scan_failed {
        tracing::info!(
            requeued = summary.requeued,
            orphaned = summary.orphaned,
            touched = summary.touched,
            unreadable = summary.unreadable,
            scan_failed = summary.scan_failed,
            "verification recovery settled"
        );
    }
    summary
}

/// Typed summary of the startup queue-head recovery.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct QueueRecoverySummary {
    candidates: usize,
    /// Sessions whose live queue still carried a runnable head.
    runnable: usize,
    /// Sessions whose durable queue state could not be read. They are KICKED
    /// ANYWAY (a read error is never an empty queue) and logged typed.
    unreadable: usize,
    /// The candidate scan itself failed: no kick could be computed here (the
    /// durable rows stay pending for the next boot).
    scan_failed: bool,
}

/// Daemon startup queue-head recovery (boundary race): a killed process can
/// leave a durable non-terminal prompt-queue row whose active turn already
/// settled; without a runner the row would wait for the next submit or settle
/// (durable, but stalled). The durable row itself is the runnable marker
/// (`Store::sessions_with_pending_queues`), so the executor's recovery entry
/// starts/arms exactly one bounded runner per session carrying one — never a
/// new submit, never a second concurrent drive, never an unbounded wait.
///
/// Runs BEFORE readiness, after `agent.recover()` and the verification
/// requeue, with the session store, manager and executor fully open. A store
/// read error is loud and NEVER classified as "already drained": the session
/// is handed to the executor's recovery entry anyway (which re-checks and
/// kicks, never releasing on a read error). A drive registry that refuses
/// the spawn is logged by the executor itself — those rows stay durably
/// pending for the next recovery, so nothing is ever lost.
fn recover_pending_queues_at_startup(graph: &DaemonGraph) -> QueueRecoverySummary {
    let mut summary = QueueRecoverySummary::default();
    let candidates = match graph.session.store().sessions_with_pending_queues() {
        Ok(sessions) => sessions,
        Err(e) => {
            summary.scan_failed = true;
            tracing::error!(
                error = %e,
                "queue recovery could not scan sessions; every durable queue row stays pending \
                 for the next boot: {e}"
            );
            return summary;
        }
    };
    summary.candidates = candidates.len();
    for session in &candidates {
        match graph.session.get_session(*session) {
            Ok(Some(handle)) => match handle.queued_prompt_count() {
                Ok(count) if count > 0 => summary.runnable += 1,
                Ok(_) => {}
                Err(e) => {
                    summary.unreadable += 1;
                    summary.runnable += 1;
                    tracing::error!(
                        session = %session,
                        error = %e,
                        "queue recovery could not read session {session}'s durable queue head; \
                         treating it as NON-EMPTY and handing it to the runner: {e}"
                    );
                }
            },
            Ok(None) => {
                summary.unreadable += 1;
                tracing::error!(
                    session = %session,
                    "queue recovery found a listed session with no handle (store inconsistency); \
                     its durable queue row stays pending"
                );
            }
            Err(e) => {
                summary.unreadable += 1;
                summary.runnable += 1;
                tracing::error!(
                    session = %session,
                    error = %e,
                    "queue recovery could not open session {session}; handing its durable queue \
                     head to the runner anyway: {e}"
                );
            }
        }
    }
    graph.tasks.recover_pending_queues();
    if summary.candidates > 0 || summary.scan_failed {
        tracing::info!(
            candidates = summary.candidates,
            runnable = summary.runnable,
            unreadable = summary.unreadable,
            scan_failed = summary.scan_failed,
            "queue recovery: durable queue heads handed to runners"
        );
    }
    summary
}

/// The daemon verification executor (audit P0-5/26 production wiring): a
/// background loop that claims Queued durable verification jobs of every
/// session, executes them through the REAL [`faktor_agent::AgentRuntime`]
/// execution primitive (claim -> execute -> resolve), and then re-settles a
/// run whose EXACT root verification attempt became terminal. Without this
/// executor a pending attempt would only ever settle on the next genuine
/// turn; with it, an ordinary background check resolves asynchronously and
/// the task's completion waits for exactly that result.
///
/// Store read errors are logged TYPED and deduplicated (this loop ticks 4x/s;
/// an unchanged error logs once, a changed one immediately) — a broken store
/// is never an invisible `continue`.
fn spawn_verification_executor(graph: &DaemonGraph) -> tokio::task::JoinHandle<()> {
    let session = graph.session.clone();
    let agent = graph.agent.clone();
    let tasks = graph.tasks.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_scan_error: Option<String> = None;
        let mut last_open_error: Option<String> = None;
        loop {
            tick.tick().await;
            let ids = match session.store().session_ids() {
                Ok(ids) => {
                    last_scan_error = None;
                    ids
                }
                Err(e) => {
                    // Deduplicated loudness: the durable queues are untouched
                    // and the next tick retries; an unchanged failure logs
                    // once instead of flooding 4x/s.
                    let message = e.to_string();
                    if last_scan_error.as_deref() != Some(message.as_str()) {
                        tracing::error!(
                            error = %message,
                            "verification executor could not scan sessions; durable jobs stay \
                             queued and the next tick retries: {message}"
                        );
                        last_scan_error = Some(message);
                    }
                    continue;
                }
            };
            for sid in ids {
                let handle = match session.get_session(sid) {
                    Ok(Some(handle)) => {
                        last_open_error = None;
                        handle
                    }
                    Ok(None) => {
                        let message = format!("session {sid} listed with no handle");
                        if last_open_error.as_deref() != Some(message.as_str()) {
                            tracing::error!(
                                session = %sid,
                                "verification executor found a listed session with no handle \
                                 (store inconsistency); its durable jobs stay queued"
                            );
                            last_open_error = Some(message);
                        }
                        continue;
                    }
                    Err(e) => {
                        let message = e.to_string();
                        if last_open_error.as_deref() != Some(message.as_str()) {
                            tracing::error!(
                                session = %sid,
                                error = %message,
                                "verification executor could not open session {sid} (its durable \
                                 jobs stay queued): {message}"
                            );
                            last_open_error = Some(message);
                        }
                        continue;
                    }
                };
                let resolved = match agent.execute_open_verification_jobs(&handle).await {
                    Ok(resolved) => resolved,
                    // No current attempt / unresolvable root: nothing to
                    // execute (never an error loop). Debug-typed so a real
                    // refusal is still traceable.
                    Err(e) => {
                        tracing::debug!(
                            session = %sid,
                            error = %e,
                            "verification executor found nothing to execute for session {sid}"
                        );
                        continue;
                    }
                };
                if resolved == 0 {
                    continue;
                }
                if let Err(e) = tasks.settle_resolved_verifications(sid).await {
                    tracing::warn!("post-executor settlement of session {sid} failed: {e}");
                }
            }
        }
    })
}

/// Resolve one ENABLED section's database path. The section's resolver
/// returns `None` only for a disabled section; reaching this helper with
/// `None` means the enabled guard and the resolver disagree (a config
/// combination no `unreachable!` may turn into a startup abort). Refuse with
/// a typed CONFIG error naming the section, never a panic and never an
/// implicit fallback path.
fn enabled_section_db_path(
    section: &str,
    resolved: Option<std::path::PathBuf>,
) -> Result<std::path::PathBuf, String> {
    resolved
        .ok_or_else(|| format!("{section} config: the enabled section resolved no database path"))
}

/// The frozen CHILD digest-attestation line the release bootstrap launcher
/// requires from a non-unix launch before it reports readiness:
/// `faktor release digest=<64 lowercase hex>`. The launcher's parser
/// (`faktor_updater::release::CHILD_DIGEST_PREFIX`) mirrors this prefix
/// because the server cannot depend on the updater's private constants; the
/// two strings must stay byte-identical.
const RELEASE_DIGEST_ATTESTATION_PREFIX: &str = "faktor release digest=";

/// The typed outcome of one startup release-digest attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReleaseAttestation {
    /// The bootstrap launcher exported no expected digest (`FAKTOR_RELEASE_DIGEST`
    /// unset, the unix/direct-start case): nothing was printed.
    NotRequested,
    /// Exactly one attestation line carrying the RUNNING binary's digest was
    /// printed. `matches_expected` is false when the launcher's export differs
    /// — still the ACTUAL digest (never a lie); the launcher then refuses.
    Printed { matches_expected: bool },
    /// No attestation is possible (malformed export or unreadable digest):
    /// nothing was printed and the loud typed error names why.
    Unavailable { reason: &'static str },
}

/// Emit the ONE release-digest attestation line when — and only when — the
/// release bootstrap launcher exported [`faktor_updater::RELEASE_DIGEST_ENV`]
/// for this child. The digest is computed with the SAME helper the health
/// build report uses ([`faktor_updater::self_digest`]: the streamed sha256 of
/// `current_exe`, i.e. this very binary), never a value copied from the
/// environment: the launcher accepts readiness only from the child's own
/// claim, so a mismatch must be observable, never papered over.
///
/// Honesty rules:
/// - a valid-length export is always answered with the ACTUAL digest, even
///   when it differs from the launcher's expected value, plus a loud typed
///   `faktor.release_digest.mismatch` error (the launcher refuses the launch);
/// - an export whose length cannot match a sha256 hex is a malformed request:
///   nothing is printed (a line the launcher must reject is never minted) and
///   a loud typed `faktor.release_digest.malformed_expected` error is logged;
/// - an unreadable self-digest is a loud typed
///   `faktor.release_digest.unreadable` error and no line.
///
/// The line carries no secrets (it is a hash of a public artifact) and is
/// written to stdout BEFORE the daemon binds/serves: [`serve_impl`] emits it
/// first and only then the frozen startup line, so a supervisor can never
/// observe readiness before the child's own digest claim.
fn emit_release_digest_attestation(
    out: &mut impl std::io::Write,
    expected: Option<&str>,
) -> std::io::Result<ReleaseAttestation> {
    let Some(expected) = expected else {
        return Ok(ReleaseAttestation::NotRequested);
    };
    let actual = match faktor_updater::self_digest() {
        Ok(actual) => actual,
        Err(e) => {
            tracing::error!(
                event = "faktor.release_digest.unreadable",
                "cannot attest the running binary digest ({e}); the bootstrap launcher will refuse this launch"
            );
            return Ok(ReleaseAttestation::Unavailable {
                reason: "self digest unreadable",
            });
        }
    };
    // The launcher parses exactly one sha256 hex length. An export of another
    // shape can never be attested honestly: log the typed refusal instead of
    // printing a line the launcher must reject as no attestation.
    if expected.len() != actual.len() {
        tracing::error!(
            event = "faktor.release_digest.malformed_expected",
            expected_len = expected.len(),
            actual_len = actual.len(),
            "the bootstrap launcher exported a malformed release digest; no attestation is possible"
        );
        return Ok(ReleaseAttestation::Unavailable {
            reason: "malformed expected digest",
        });
    }
    writeln!(out, "{RELEASE_DIGEST_ATTESTATION_PREFIX}{actual}")?;
    out.flush()?;
    let matches_expected = actual == expected;
    if !matches_expected {
        tracing::error!(
            event = "faktor.release_digest.mismatch",
            expected = %expected,
            actual = %actual,
            "the running binary does not match the bootstrap-verified release digest; the launcher will refuse this launch"
        );
    }
    Ok(ReleaseAttestation::Printed { matches_expected })
}

/// Adversarial covers of the daemon-side release digest attestation: the env
/// gate, the exactly-one-line/actual-digest/ordering contract, the loud typed
/// mismatch log and the digest-helper reuse proven against the health report.
#[cfg(test)]
mod release_attestation_tests {
    use super::*;

    /// A capturing `MakeWriter` sink for the loud typed error assertions.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = CapturedLog;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Run `f` with a thread-local ERROR-level fmt subscriber whose output is
    /// captured, returning the value and the log text.
    fn capture_errors<T>(f: impl FnOnce() -> T) -> (T, String) {
        let sink = CapturedLog::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::ERROR)
            .finish();
        let value = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8(
            sink.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .expect("the captured log is UTF-8");
        (value, logs)
    }

    /// The unix/direct-start case: no export means NOTHING may appear on
    /// stdout (the frozen startup line stays the only line).
    #[test]
    fn an_unset_export_prints_no_attestation_line() {
        let mut out = Vec::new();
        let outcome = emit_release_digest_attestation(&mut out, None).unwrap();
        assert_eq!(outcome, ReleaseAttestation::NotRequested);
        assert!(out.is_empty(), "no line when env is unset: {out:?}");
    }

    /// A requested attestation is EXACTLY one line, 64 lowercase hex, the
    /// actual running-binary digest, and it precedes the frozen startup line
    /// (the order [`serve_impl`] writes: attestation, then bind, then the
    /// startup line that flips readiness).
    #[test]
    fn a_requested_export_prints_exactly_one_actual_line_before_readiness() {
        let actual = faktor_updater::self_digest().expect("the test binary hashes");
        assert!(
            faktor_updater::manifest::is_lower_hex(&actual, 64),
            "the helper yields 64 lowercase hex: {actual}"
        );
        let mut out = Vec::new();
        let outcome = emit_release_digest_attestation(&mut out, Some(&actual)).unwrap();
        assert_eq!(
            outcome,
            ReleaseAttestation::Printed {
                matches_expected: true
            }
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            format!("faktor release digest={actual}\n"),
            "exactly one line carrying the actual digest"
        );
        // Digest-helper reuse: the SAME value the health build report folds
        // into `self_sha256` (the running-binary digest a supervisor reads).
        let report = build_report_json(true);
        assert_eq!(report["self_sha256"].as_str(), Some(actual.as_str()));
    }

    /// The launcher-refusal path: the export names another digest. The daemon
    /// still prints the ACTUAL digest (never the expected one), logs the loud
    /// typed mismatch, and never fabricates an attestation.
    #[test]
    fn a_mismatched_export_prints_the_actual_digest_and_logs_loudly() {
        let actual = faktor_updater::self_digest().expect("the test binary hashes");
        let expected = "0".repeat(64);
        assert_ne!(
            actual, expected,
            "the fixture must differ from the real hash"
        );
        let mut out = Vec::new();
        let (outcome, logs) =
            capture_errors(|| emit_release_digest_attestation(&mut out, Some(&expected)).unwrap());
        assert_eq!(
            outcome,
            ReleaseAttestation::Printed {
                matches_expected: false
            }
        );
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, format!("faktor release digest={actual}\n"));
        assert!(
            !text.contains(&expected),
            "the expected value is never attested: {text}"
        );
        assert!(
            logs.contains("faktor.release_digest.mismatch"),
            "the mismatch is a loud typed error: {logs}"
        );
        assert!(logs.contains(&expected) && logs.contains(&actual));
    }

    /// A malformed export can never be answered honestly: no line is minted
    /// (the launcher would reject it anyway) and the refusal is loud + typed.
    #[test]
    fn a_malformed_export_never_prints_a_minted_line() {
        let mut out = Vec::new();
        let (outcome, logs) =
            capture_errors(|| emit_release_digest_attestation(&mut out, Some("abc")).unwrap());
        assert_eq!(
            outcome,
            ReleaseAttestation::Unavailable {
                reason: "malformed expected digest"
            }
        );
        assert!(out.is_empty(), "no line for a malformed export: {out:?}");
        assert!(
            logs.contains("faktor.release_digest.malformed_expected"),
            "the malformed export is a loud typed error: {logs}"
        );
    }
}

/// Shared daemon serve core (audit 44 ordering): `agent.recover()` -> bind
/// -> print the frozen startup line -> spawn the gated backup task. A backup
/// can NEVER delay readiness: the task is spawned only after the startup
/// line, waits [`BACKUP_START_DELAY`], and is gated by policy.
///
/// `ready_tx` fires right after the startup line is printed (test probe for
/// the "startup line before any backup file exists" ordering guarantee);
/// `shutdown_rx`, when present, ends the daemon (the test harness's
/// deterministic trigger). Production passes `None`: the daemon then waits
/// for SIGTERM/SIGINT, runs the SAME [`shutdown_serving_daemon`] drain, and
/// arms a second-signal watchdog that force-exits (code 130) if the operator
/// insists while the bounded drain is still running. A signal can never kill
/// the process mid-write without the drain/backup/index joins.
#[allow(clippy::too_many_arguments)]
async fn serve_impl(
    port: u16,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
    shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
) -> Result<(), String> {
    let (config, semantic) = match serve_config_and_semantic(config_path) {
        Ok(loaded) => loaded,
        Err(e) => return Err(format!("config error: {e}")),
    };
    // Live chunk path (audit 41): BOUNDED channel (1024 events) + sink-side
    // coalescing under backpressure — a slow SSE consumer can never grow
    // the agent's memory. The drainer spawn lives in serve().
    let (chunk_sink, chunk_rx) = faktor_agent::ChunkSink::channel();
    // The whole graph is built HERE in the construction region (steps
    // 1-16 of graph::DAEMON_CONSTRUCTION_ORDER): store/session + CAS →
    // supervisor → checked transport → providers → router → budgets →
    // index → evidence → instructions → verification → agent →
    // orchestrator → shadows → tasks. Serve constructs NOTHING of its own.
    // The `[cloud]` section is resolved into the daemon's own data dir
    // BEFORE the config is consumed: when disabled (the default and every
    // pre-cloud config) the control-plane/SCM databases are neither created
    // nor opened, and the local daemon stays byte-identical. When enabled,
    // the control plane and the durable SCM rows are opened here (their
    // files live under the data dir; a hostile path is refused at startup).
    // The additive `[billing]` section (Wave 3) is captured BEFORE the
    // config is consumed by the daemon build: when disabled (the default)
    // no billing database is created and every billing route answers 409.
    let config_billing = config.billing.clone();
    // The additive `[workers]` section (remote/VPC worker plane) is captured
    // the same way: disabled by default (no worker database, every
    // /native/workers* route 409 `workers_disabled`, the TaskExecutor
    // placement seam disabled => local execution unchanged).
    let config_workers = config.workers.clone();
    // The additive `[worker_plane]` section (the worker-plane DEPLOYMENT
    // BOUNDARY) is captured the same way: disabled by default (no second
    // socket; the native listener is untouched). When enabled, a dedicated
    // listener with its own identity is bound under the TLS-or-gateway-only
    // rules and the typed refusal is raised BEFORE the daemon serves.
    let config_worker_plane = config.worker_plane.clone();
    // The additive `[enterprise]` section (retention/audit/admin plane) is
    // captured the same way: disabled by default (no enterprise database,
    // every /native/enterprise/* route 409 `enterprise_disabled`, the local
    // daemon otherwise untouched).
    let config_enterprise = config.enterprise.clone();
    // The `[cloud]` section is captured BEFORE the config is consumed too:
    // the SSO authority and the GitHub App surface are constructed after the
    // graph (they ride the graph's checked transport), and both read the
    // operator-staged payload directory. Disabled sections build nothing.
    let config_cloud = config.cloud.clone();
    let cloud_databases = if config.cloud.enabled {
        let control_plane_path = config
            .cloud
            .control_plane_path(&data_dir)
            .map_err(|e| format!("cloud config: {e}"))?;
        let scm_path = config
            .cloud
            .scm_path(&data_dir)
            .map_err(|e| format!("cloud config: {e}"))?;
        Some((control_plane_path, scm_path))
    } else {
        None
    };
    // The `[updater]` section is resolved BEFORE the config is consumed:
    // when disabled (the default and every pre-updater config) NO operation
    // database and NO install root are ever created, and the daemon is
    // byte-identical to the pre-updater daemon. When enabled, the durable
    // operation store, the content-addressed install layout and the checked
    // download transport (the `[sandbox]` destination policy) are built
    // here; artifacts download through the daemon's own egress policy.
    let updater = if config.updater.enabled {
        let db = config
            .updater
            .database_path(&data_dir)
            .map_err(|e| format!("updater config: {e}"))?
            .ok_or("updater config: enabled section resolved no database")?;
        let install_root = config
            .updater
            .install_root_path(&data_dir)
            .map_err(|e| format!("updater config: {e}"))?
            .ok_or("updater config: enabled section resolved no install root")?;
        let policy = config
            .sandbox_policy()
            .map_err(|e| format!("sandbox config: {e}"))?
            .network
            .installed()
            .cloned()
            .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
        let store = Arc::new(
            faktor_updater::SqliteUpdaterStore::open(&db)
                .map_err(|e| format!("updater store {}: {e}", db.display()))?,
        );
        let fetcher = Arc::new(faktor_updater::CheckedHttpFetcher::new(
            faktor_provider::egress::CheckedHttpClient::try_with_policy(policy)
                .map_err(|e| format!("updater egress client: {e}"))?,
        ));
        let updater_config = faktor_updater::UpdaterConfig {
            channel: config
                .updater
                .channel()
                .map_err(|e| format!("updater config: {e}"))?,
            install_root,
            keys: config
                .updater
                .trusted_keys()
                .map_err(|e| format!("updater config: {e}"))?,
            max_artifact_bytes: config.updater.max_artifact_bytes_resolved(),
            clock_skew_ms: config.updater.clock_skew_ms_resolved(),
            host_os: std::env::consts::OS.to_string(),
            host_arch: std::env::consts::ARCH.to_string(),
            local_version: faktor_core::VERSION.to_string(),
            allow_legacy_manifests_once: config.updater.allow_legacy_manifests_once_resolved(),
        };
        let updater = faktor_updater::Updater::with_default_probe(updater_config, store, fetcher)
            .map_err(|e| format!("updater init: {e}"))?;
        Some(Arc::new(updater))
    } else {
        None
    };
    let graph =
        build_daemon_with_mcp_and_chunks_fast(&data_dir, Some(config), Some(chunk_sink), semantic)
            .await
            .map_err(|e| format!("daemon build failed: {e}"))?;
    let session = graph.session.clone();
    let agent = graph.agent.clone();
    let store = session.store();
    // Crash recovery runs before the first request (spec §7) — and before
    // bind, so all recovery work is done before readiness is announced.
    if let Err(e) = agent.recover() {
        tracing::error!("recovery failed: {e}");
    }
    // Durable verification-job recovery: stale Running rows of a previous
    // daemon are requeued BEFORE the executor can claim anything, so a
    // crashed check is retried rather than lost.
    recover_verification_jobs_at_startup(&session);
    // Durable queue-head recovery (boundary race): a pending prompt-queue row
    // whose turn already settled gets its runner now — before bind, so the
    // first request sees a daemon that has already re-attached every durable
    // head no new submit would have kicked.
    recover_pending_queues_at_startup(&graph);
    // Step 17 — the server surface consumes the graph's authorities: the
    // orchestrator and the TaskExecutor of ServerDeps are the SAME
    // instances the graph built (no second execution authority is ever
    // constructed in a daemon lifetime). The graph stays alive for the
    // whole serve below, so every authority — supervisor, shadow service,
    // budget ledger, index — lives exactly as long as the daemon.
    let mut deps = ServerDeps::new_with(
        session,
        agent,
        graph.permissions.clone(),
        graph.orchestrator.clone(),
        graph.tasks.clone(),
        graph.budgets.clone(),
    );
    deps.chunk_rx = Some(chunk_rx);
    // The additive real GitHub App surface (built only while the section is
    // enabled; its initial sync is backgrounded after readiness below).
    let mut scm_daemon: Option<Arc<crate::scm_daemon::ScmDaemon>> = None;
    if let Some((control_plane_path, scm_path)) = cloud_databases {
        let control_plane_path =
            enabled_section_db_path("cloud control-plane", control_plane_path)?;
        let control_plane = Arc::new(faktor_cloud::ControlPlane::new(
            Arc::new(
                faktor_cloud::SqliteControlPlaneStore::open(&control_plane_path).map_err(|e| {
                    format!(
                        "cloud control-plane store {}: {e}",
                        control_plane_path.display()
                    )
                })?,
            ),
            Arc::new(faktor_cloud::SystemClock),
        ));
        let scm_path = enabled_section_db_path("cloud scm", scm_path)?;
        let scm = {
            let store = faktor_scm::SqliteScmStore::open(&scm_path)
                .map_err(|e| format!("cloud scm store {}: {e}", scm_path.display()))?;
            Arc::new(store) as Arc<dyn faktor_scm::ScmStore>
        };
        let scm_store = scm.clone();
        deps = deps.with_control_plane(control_plane).with_scm_store(scm);
        // The GitHub App wiring rides the graph's ONE checked transport and
        // the operator-staged payload directory; a missing/corrupt/
        // too-permissive payload is a startup refusal (no half-wired SCM).
        scm_daemon = crate::scm_daemon::build_scm_daemon(
            &config_cloud,
            &data_dir,
            scm_store,
            graph.transport.clone(),
        )
        .map_err(|e| format!("cloud scm wiring: {e}"))?;
        if let Some(daemon) = &scm_daemon {
            deps = deps.with_scm_webhook(daemon.clone());
            tracing::info!("github app scm surface enabled");
        }
        tracing::info!("cloud control plane enabled");
    }
    // The additive `[cloud.sso]` section: the network OIDC adapter over the
    // daemon's checked transport plus the operator-staged client secret.
    // Disabled (the default) = no adapter is built and /native/sso/* keeps
    // its typed 409 parity.
    if let Some(sso_cfg) = config_cloud.sso.as_ref().filter(|sso| sso.enabled) {
        let payload_root = config_cloud
            .payload_root(&data_dir)
            .map_err(|e| format!("cloud config: {e}"))?;
        let authority =
            crate::sso_auth::build_sso_authority(sso_cfg, &payload_root, graph.transport.clone())
                .map_err(|e| format!("cloud sso wiring: {e}"))?;
        deps = deps.with_sso(authority);
        tracing::info!("sso authority enabled");
    }
    // The additive `[billing]` section (Wave 3): when disabled (the default)
    // no billing database is created and every billing route answers 409.
    // When enabled, the durable usage/credit ledger opens here (its own
    // `billing.db` through the SAME control-plane store + migration ladder),
    // the configured account is provisioned idempotently, and the
    // orchestrator's admission gate is wired to the entitlement service —
    // the gate is consulted at the three safe boundaries only, so an
    // entitlement change can never interrupt an in-flight
    // integration/rollback/completion transaction.
    let mut billing_gate: Option<Arc<dyn faktor_orchestrator::admission::AdmissionGate>> = None;
    // The additive `[billing.report]` schedule (disabled by default): a
    // durable period/cursor schedule that reports the folded usage through
    // the vendor adapter after readiness.
    let mut billing_report: Option<Arc<crate::billing_report::BillingReportRunner>> = None;
    if config_billing.enabled {
        let path = config_billing
            .billing_path(&data_dir)
            .map_err(|e| format!("billing config: {e}"))?;
        let organization = config_billing
            .organization()
            .map_err(|e| format!("billing config: {e}"))?;
        let account = config_billing
            .account_id()
            .map_err(|e| format!("billing config: {e}"))?;
        let service_config = config_billing
            .service_config()
            .map_err(|e| format!("billing config: {e}"))?
            .ok_or_else(|| {
                "billing config: an enabled section carries a service config".to_string()
            })?;
        let store = {
            let path = enabled_section_db_path("billing", path)?;
            Arc::new(
                faktor_cloud::SqliteControlPlaneStore::open(&path)
                    .map_err(|e| format!("billing store {}: {e}", path.display()))?,
            ) as Arc<dyn faktor_cloud::BillingStore>
        };
        let service = faktor_cloud::EntitlementService::with_system_clock(store, service_config)
            .map_err(|e| format!("billing service: {e}"))?;
        // The report schedule (additive; disabled by default): strict vendor
        // config, the durable billing store and the daemon's checked
        // transport. A missing vendor credential env var refuses startup —
        // never a silently unauthenticated report.
        if let Some(report_cfg) = config_billing
            .report
            .as_ref()
            .filter(|report| report.enabled)
        {
            let vendor_config = report_cfg.vendor_config()?;
            let policy = report_cfg.policy()?;
            let adapter = if report_cfg.unauthenticated() {
                faktor_cloud::BillingVendorAdapter::new(
                    service.clone(),
                    vendor_config,
                    graph.transport.clone(),
                    None,
                )
            } else {
                faktor_cloud::BillingVendorAdapter::from_env(
                    service.clone(),
                    vendor_config,
                    graph.transport.clone(),
                )
            }
            .map_err(|e| format!("billing report config: {e}"))?;
            billing_report = Some(Arc::new(crate::billing_report::BillingReportRunner::new(
                service.store().clone(),
                adapter,
                organization.clone(),
                policy,
                Arc::new(faktor_cloud::SystemClock),
            )));
            tracing::info!("billing report schedule enabled");
        }
        let account_name = config_billing
            .account_name
            .clone()
            .unwrap_or_else(|| "local".to_string());
        let managed = config_billing.managed.unwrap_or(true);
        service
            .ensure_account(&organization, &account, &account_name, managed)
            .map_err(|e| format!("billing account provisioning: {e}"))?;
        // Wave 5 residual: install the agent-side debit authority, so a
        // Faktor-managed provider attempt opens its durable credit hold
        // BEFORE dispatch (record-before-call) and settles/refunds with the
        // reservation ledger; BYOK providers are classified and never
        // debited. Without this install the agent dispatch path is exactly
        // the pre-billing path (no debit call exists).
        // A poisoned authority slot must refuse daemon startup (fail-closed):
        // silently continuing would run managed traffic without the debit
        // authority it is configured to have.
        graph
            .agent
            .set_provider_debits(Some(Arc::new(billing_debits::CloudAttemptDebits::new(
                service.clone(),
                organization.clone(),
                account.clone(),
            ))))
            .map_err(|e| format!("installing the agent debit authority: {e}"))?;
        billing_gate = Some(Arc::new(BillingAdmissionGate::new(
            service.clone(),
            organization,
        )));
        deps = deps.with_billing(service);
        tracing::info!("commercial billing enabled");
    }
    if let Some(gate) = billing_gate {
        graph.orchestrator.set_admission_gate(gate);
    }
    // The additive `[workers]` section: when disabled (the default) nothing
    // is built and no worker database is created. When enabled, the durable
    // plane opens under the data dir, its crash recovery runs BEFORE the
    // first request (expired leases requeue deterministically; unleased
    // assignments are re-attempted), the worker routes are wired, and the
    // TaskExecutor gains the placement seam so a new run is placed on an
    // eligible registered worker instead of executing locally.
    if config_workers.enabled {
        let path = config_workers
            .workers_path(&data_dir)
            .map_err(|e| format!("workers config: {e}"))?;
        let organization = config_workers
            .organization()
            .map_err(|e| format!("workers config: {e}"))?;
        let trust_domain = config_workers
            .trust_domain()
            .map_err(|e| format!("workers config: {e}"))?;
        let requirements = config_workers
            .requirements()
            .map_err(|e| format!("workers config: {e}"))?;
        let store = {
            let path = enabled_section_db_path("workers", path)?;
            Arc::new(
                faktor_worker::SqliteWorkerStore::open(&path)
                    .map_err(|e| format!("worker store {}: {e}", path.display()))?,
            ) as Arc<dyn faktor_worker::WorkerStore>
        };
        let plane = faktor_worker::WorkerPlane::with_system_clock(store);
        // Crash recovery before the first request: a lease whose heartbeat
        // window closed while the daemon was down expires (attempt terminal
        // + bounded requeue), and an assignment committed without its lease
        // is re-attempted. Idempotent: a clean restart reports nothing.
        let recovery = plane
            .recover(&organization)
            .map_err(|e| format!("worker recovery: {e}"))?;
        let resumed = plane
            .resume_assignments(&organization)
            .map_err(|e| format!("worker assignment recovery: {e}"))?;
        if !recovery.expired.is_empty() || !resumed.reassigned.is_empty() {
            tracing::warn!(
                expired = recovery.expired.len(),
                requeued = recovery.requeued.len(),
                exhausted = recovery.exhausted.len(),
                reassigned = resumed.reassigned.len(),
                "worker plane recovery resolved interrupted leases/assignments"
            );
        }
        graph
            .tasks
            .set_worker_placement(faktor_orchestrator::placement::WorkerPlacement::enabled(
                Arc::new(WorkerPlaneAdapter {
                    plane: plane.clone(),
                    organization,
                    trust_domain,
                    requirements,
                }),
            ));
        deps = deps.with_workers(plane);
        tracing::info!("remote/vpc worker plane enabled");
    }
    // The additive `[enterprise]` section: when disabled (the default)
    // nothing is built and no enterprise database is created. When enabled,
    // the retention/audit/admin service opens its own `enterprise.db`
    // (through the SAME control-plane store + migration ladder; the audit
    // table is append-only) and the GC route is wired to the daemon's REAL
    // session store + CAS, so protected rollback material is refused at the
    // scanner and again at the storage layer. An enabled section requires
    // the cloud section (the principal source); the pair is refused at
    // config load, so reaching here without it is a bug.
    if config_enterprise.enabled {
        if deps.control_plane.is_none() {
            return Err(
                "enterprise: an enabled [enterprise] section requires [cloud] enabled".into(),
            );
        }
        let path = config_enterprise
            .enterprise_path(&data_dir)
            .map_err(|e| format!("enterprise config: {e}"))?
            .ok_or_else(|| "enterprise config: enabled without a database path".to_string())?;
        let store = Arc::new(
            faktor_cloud::SqliteControlPlaneStore::open(&path)
                .map_err(|e| format!("enterprise store {}: {e}", path.display()))?,
        ) as Arc<dyn faktor_cloud::EnterpriseStore>;
        let service = Arc::new(
            faktor_cloud::EnterpriseService::with_system_clock(store)
                .map_err(|e| format!("enterprise service: {e}"))?,
        );
        let runtime = Arc::new(faktor_server::native::enterprise::RetentionRuntime::new(
            deps.session.store(),
            deps.session.cas(),
        ));
        deps = deps.with_enterprise(service, Some(runtime));
        tracing::info!("enterprise retention/audit plane enabled");
    }
    // The signed updater (additive; disabled by default): crash recovery
    // runs BEFORE the first request — an interrupted apply is resumed or
    // rolled back, never left half-applied. The daemon probe re-hashes the
    // swapped artifact (the CLI-local `faktor updater apply` runs the real
    // doctor quick check instead). When THIS process was launched through
    // the bootstrap launcher, its verified release digest is passed to
    // recovery: a release activation whose pointer is in place is only
    // resumed as applied when the running process IS the activated release.
    if let Some(updater) = &updater {
        let running_release_digest = faktor_updater::running_release().map(|(_, digest)| digest);
        match updater.recover_with_running_digest(now_ms(), running_release_digest.as_deref()) {
            Ok(outcomes) if !outcomes.is_empty() => {
                for outcome in &outcomes {
                    tracing::warn!(
                        "updater recovery resolved an interrupted operation: {outcome:?}"
                    );
                }
            }
            Ok(_) => {}
            Err(e) => tracing::error!("updater recovery failed: {e}"),
        }
        deps = deps.with_updater(updater.clone());
        tracing::info!(
            "signed updater enabled (channel {})",
            updater.config().channel
        );
    }
    // The running-build report (audit: activation must be observable as a
    // PROCESS fact, not metadata): the daemon logs one bounded JSON line to
    // stderr and — when the bootstrap launcher verified a release — folds
    // the release id + digest into `version`, so `/native/health` names the
    // exact artifact a supervisor is talking to. A directly started binary
    // reports no release identity (honest absence).
    tracing::info!("faktor build: {}", build_report_json(false));
    if let Some((release_id, digest)) = faktor_updater::running_release() {
        deps.version = format!("{}+release.{release_id}.{digest}", faktor_core::VERSION);
    }
    // The ONE semantic-provider registry: the SAME Arc the graph built and
    // the agent holds — the native introspection endpoints inspect only
    // `deps.semantic` (no parallel registry exists anywhere).
    deps = deps.with_semantic_registry(graph.semantic.clone());
    // The frontend generates the secret and passes it via env; the
    // daemon reads it here and never prints it. An explicitly supplied
    // value that is not the canonical 64-hex form fails startup loudly —
    // never a silent downgrade to weaker auth.
    deps.server_password = ServerPassword::try_from_env().map_err(|e| e.to_string())?;
    // The workspace root rides the global event envelope.
    deps.directory = std::env::current_dir()
        .ok()
        .map(|d| d.display().to_string());
    // Wire the native snapshot store so the wire revert/unrevert/diff
    // endpoints restore real files: the checkpoint store shares the
    // daemon's store + CAS (same rows, same blobs).
    let fs = faktor_fs::WorkspaceFileService::new();
    let snapshots = Arc::new(faktor_snapshot::CheckpointStore::new(
        deps.session.cas(),
        deps.session.store(),
    ));
    deps = deps.with_snapshots(fs, snapshots);
    // Wire the daemon's DURABLE evidence store of record (audit 82/CCR):
    // the native server receives the graph's ONE authority (the SAME `Arc`
    // the runtime's ContextCompiler selects from and the runtime's archiver
    // inserts into). No parallel authority is constructed here, so ids are
    // globally unique across restart, scope checks are enforced by the one
    // authority, and a foreign session can never read even knowing a
    // backing digest.
    deps = deps.with_evidence_store(Arc::new(std::sync::RwLock::new(Box::new(
        graph.evidence.clone(),
    ))));
    // Bind BEFORE readiness and BEFORE any backup work (audit 44): the
    // historic code ran rotate_backup synchronously between recover() and
    // bind, so a slow or cold backup delayed first-request readiness.
    // The additive `[worker_plane]` deployment boundary: resolve the typed
    // bind/transport config (a refusal here is a STARTUP refusal naming the
    // boundary) and bind the dedicated listener BEFORE the native listener,
    // so a refused boundary never leaves a half-started daemon. The native
    // listener stays loopback-only and reads none of these keys.
    let worker_plane_config = config_worker_plane
        .resolve()
        .map_err(|e| format!("worker plane config: {e}"))?;
    // The dedicated listener's daemon-queryable slot (see
    // [`faktor_server::api::WorkerPlaneListener`]): created BEFORE the deps
    // envelope is shared and installed with the bound handle before the
    // native listener starts, so `/native/health` reports the typed worker
    // plane state (serving / unavailable / stopped / disabled) and never
    // observes an enabled-but-uninstalled plane.
    let worker_plane_listener = worker_plane_config
        .as_ref()
        .map(|_| faktor_server::api::WorkerPlaneListener::default());
    if let Some(listener) = &worker_plane_listener {
        deps = deps.with_worker_plane_listener(listener.clone());
    }
    // The bounded live chunk stream is drained exactly once, before the deps
    // envelope is shared between the two listeners.
    faktor_server::drain_chunk_stream(&mut deps);
    let deps = std::sync::Arc::new(deps);
    if let Some(worker_plane_config) = worker_plane_config {
        let handle = faktor_server::serve_worker_plane(deps.clone(), worker_plane_config)
            .await
            .map_err(|e| format!("worker plane: {e}"))?;
        // The audit record of the boundary decision (including the
        // trusted-gateway acknowledgement, when given).
        tracing::info!("{}", handle.exposure.audit_line());
        if handle.exposure.beyond_loopback {
            tracing::warn!(
                "worker plane listens on {} beyond loopback behind an acknowledged trusted gateway ({}); TLS/mTLS terminate at the gateway",
                handle.addr,
                handle.exposure.audit_line()
            );
        }
        // The slot OWNS the handle from here on: `/native/health` queries its
        // typed status, and the shutdown sequence takes it back out for the
        // bounded join (never a detached listener, never an ownerless task).
        if let Some(listener) = &worker_plane_listener {
            listener
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .install(handle);
        }
    }
    // The release digest attestation (non-unix bootstrap readiness; see
    // [`emit_release_digest_attestation`]) is written BEFORE the native
    // listener binds/serves, so the child's own digest claim can never follow
    // the readiness it proves. Only the bootstrap launcher's exported
    // `FAKTOR_RELEASE_DIGEST` requests it; a directly started daemon (the
    // unix default) prints nothing here and stdout stays byte-identical.
    {
        let requested_release_digest = std::env::var(faktor_updater::RELEASE_DIGEST_ENV).ok();
        let mut stdout = std::io::stdout().lock();
        emit_release_digest_attestation(&mut stdout, requested_release_digest.as_deref())
            .map_err(|e| format!("release digest attestation: {e}"))?;
    }
    let handle = faktor_server::serve_arc(deps, port)
        .await
        .map_err(|e| format!("failed to bind: {e}"))?;
    // The frozen stdout line; nothing else may be printed (the ONE optional
    // release-digest attestation above precedes it). Readiness is now
    // announced — no backup has run yet and, by construction, cannot have.
    println!("{}", handle.startup_line);
    tracing::info!("faktor serving on {}", handle.addr);
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }
    // Automatic online backup (spec §24), post-ready and low priority: a
    // delayed spawned task, gated by policy (audit 44: interval +
    // staleness), so it never delays startup and never piles hourly
    // snapshots onto rapid restarts. Best effort, runs exactly once. The
    // SYNC backup/rotation work runs on the blocking pool
    // (`spawn_blocking` — P0-46: the sync SQLite snapshot used to run
    // inline on a Tokio worker, stalling every other task on that worker
    // for the whole snapshot); the async wrapper only sleeps, gates, and
    // joins.
    let backup_task = spawn_startup_backup(store, data_dir.clone());
    // The daemon verification executor: post-readiness, it claims and
    // resolves durable verification jobs of every session asynchronously.
    let verification_executor = spawn_verification_executor(&graph);
    // The GitHub App surface (post-readiness, like the backup/index): ONE
    // bounded initial installation/repository sync, then one idempotent
    // re-sync per verified webhook delivery through the bounded queue.
    let scm_task = scm_daemon.map(|daemon| tokio::spawn(daemon.run()));
    // The billing report schedule (post-readiness): one bounded tick per
    // configured interval; every period is reported at most once.
    let billing_report_task = billing_report.map(|runner| tokio::spawn(runner.run()));
    // Keep the daemon alive; when a shutdown is signaled, stop the owned
    // worker-plane listener FIRST (bounded graceful join of its serve task;
    // an unexpected death is named by the typed health snapshot), stop the
    // graph-hosted repository index reconciliation worker (bounded join),
    // close and DRAIN the TaskExecutor's detached drives (bounded
    // graceful-then-abort), then drain the backup task (bounded) and the
    // owned Ollama warm-up threads before the daemon returns. The drive
    // drain runs BEFORE the backup drain so the final snapshot sees every
    // settled record-first write; a straggler aborted by the registry stays
    // resumable from its durable rows.
    match shutdown_rx {
        Some(rx) => {
            let _ = rx.await;
            shutdown_serving_daemon(
                &graph.tasks,
                Some(handle),
                worker_plane_listener,
                graph.index.clone(),
                Some(&graph.agent),
                verification_executor,
                scm_task,
                billing_report_task,
                backup_task,
            )
            .await;
        }
        None => {
            // Production: SIGTERM/SIGINT trigger the SAME drain as the test
            // harness's shutdown channel. A second signal force-exits (the
            // drain itself is bounded, this is the operator's escalate).
            let signal = wait_for_shutdown_signal().await;
            tracing::info!(signal, "shutdown signal received; draining the daemon");
            #[cfg(test)]
            SIGNAL_DRAIN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
            spawn_force_exit_watchdog();
            shutdown_serving_daemon(
                &graph.tasks,
                Some(handle),
                worker_plane_listener,
                graph.index.clone(),
                Some(&graph.agent),
                verification_executor,
                scm_task,
                billing_report_task,
                backup_task,
            )
            .await;
        }
    }
    Ok(())
}

async fn run(prompt: String, provider: &str, model: &str, workspace: PathBuf, data_dir: PathBuf) {
    match build_daemon(&data_dir, None) {
        Ok(graph) => {
            let (session, agent) = (graph.session, graph.agent);
            let ws = match session.create_workspace(workspace.to_str().unwrap_or(".")) {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("workspace error: {e}");
                    std::process::exit(1);
                }
            };
            let row = match session.create_session(ws, "cli run", provider, model) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("session error: {e}");
                    std::process::exit(1);
                }
            };
            match agent.run_turn(row.id(), &prompt, &[]).await {
                Ok(outcome) => {
                    println!("final state: {}", outcome.final_state.label());
                }
                Err(e) => {
                    eprintln!("turn error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("daemon build failed: {e}");
            std::process::exit(1);
        }
    }
}

/// The ACP agent server (`faktor-cli acp`): build the REAL daemon graph over
/// the data dir (config from `faktor-plus.json` in the data dir when present,
/// else defaults; NO MCP layer — the acp surface needs the same providers,
/// tools, session store and agent the native daemon serves) and serve the
/// ACP wire protocol on stdin/stdout until EOF or `shutdown`.
async fn acp(data_dir: PathBuf) {
    let config = load_acp_config(&data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("data dir error: {e}");
        std::process::exit(1);
    }
    // The ACP surface is stdio-only: no SSE subscribers exist, so there is
    // no chunk sink (None = the runtime skips live-chunk overhead entirely).
    // The daemon graph is built whole by [`build_daemon`] (the construction
    // region; steps 1-16) — ACP constructs no supervisor or executor of its
    // own and takes its session/agent references from the graph.
    let graph = match build_daemon(&data_dir, Some(config)) {
        Ok(graph) => graph,
        Err(e) => {
            eprintln!("daemon build failed: {e}");
            std::process::exit(1);
        }
    };
    let (session, agent) = (graph.session.clone(), graph.agent.clone());
    // Crash recovery runs before the first request (spec §7), like serve.
    if let Err(e) = agent.recover() {
        tracing::error!("recovery failed: {e}");
    }
    // Durable queue-head recovery, exactly like serve: the same order (after
    // `agent.recover()`, before the first prompt is accepted) over the same
    // executor, so a killed process's pending durable head is driven without
    // any new submit.
    recover_pending_queues_at_startup(&graph);
    // The ACP prompt surface enters the SAME product execution authority the
    // daemon server uses (the graph's ONE TaskExecutor over the ONE session
    // store) — ACP translates its wire prompts, it never drives the agent.
    let prompts =
        faktor_server::native::PromptExecutionService::new(graph.tasks.clone(), session.clone());
    // The negotiated `faktor.terminal` seam: the daemon's session-owned
    // terminal authority (the same faktor-pty + ownership-row registry the
    // native terminal surface serves) over the SAME session manager the
    // backend drives. Without this the extension is never negotiated and
    // every `terminal/*` method stays the official -32601.
    let terminal_authority: Arc<dyn faktor_acp::TerminalAuthority> =
        Arc::new(DaemonTerminalAuthority::new(session.clone()));
    let backend = DaemonAcpBackend::new(session, agent, prompts);
    match AcpServer::new(backend)
        .with_terminal_authority(terminal_authority)
        .run_stdio()
        .await
    {
        Ok(()) => {}
        Err(e) => {
            eprintln!("acp server error: {e}");
            std::process::exit(1);
        }
    }
}

/// Daemon config for `acp`: `faktor-plus.json` next to the data dir when it
/// exists; a broken file is a loud warning that falls back to defaults (the
/// daemon still serves — same policy as `serve`).
fn load_acp_config(data_dir: &std::path::Path) -> config::Config {
    let path = data_dir.join("faktor-plus.json");
    if !path.exists() {
        return config::Config::default();
    }
    match config::Config::load(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("config error: {e}; using defaults");
            config::Config::default()
        }
    }
}

/// The daemon's worker-plane placement adapter: the orchestrator's seam
/// consults it BEFORE any local durable write of a new run. It maps the
/// provider-neutral placement spec onto the worker plane:
///
/// - no eligible worker => `Local` (the run executes locally exactly as
///   before; nothing is written to the worker plane);
/// - an eligible worker => the plane mints one IMMUTABLE job generation,
///   CAS-accepts its lease and `Remote` is returned: the local executor
///   starts NOTHING and the plane's durable job/lease owns the attempt.
///
/// A plane failure is returned as a string and becomes the executor's typed
/// `ExecError::PlacementRefused` — never a silent local run.
struct WorkerPlaneAdapter {
    plane: Arc<faktor_worker::WorkerPlane>,
    organization: faktor_cloud::OrganizationId,
    trust_domain: String,
    requirements: faktor_worker::JobRequirements,
}

impl faktor_orchestrator::placement::WorkerPlacementSeam for WorkerPlaneAdapter {
    fn place(
        &self,
        spec: &faktor_orchestrator::placement::PlacementSpec,
    ) -> Result<faktor_orchestrator::placement::PlacementDecision, String> {
        let mut requirements = self.requirements.clone();
        // The executor's spec carries the run shape; the daemon's `[workers]`
        // defaults carry the environment requirements. A spec-provided field
        // wins (the executor currently sends none).
        if let Some(os) = &spec.os {
            requirements.os = Some(os.clone());
        }
        if let Some(arch) = &spec.arch {
            requirements.arch = Some(arch.clone());
        }
        if !spec.toolchains.is_empty() {
            requirements.toolchains = spec.toolchains.clone();
        }
        if let Some(region) = &spec.region {
            requirements.region = Some(region.clone());
        }
        if spec.min_cpu_cores > 0 {
            requirements.min_cpu_cores = spec.min_cpu_cores;
        }
        if spec.min_memory_mb > 0 {
            requirements.min_memory_mb = spec.min_memory_mb;
        }
        if spec.gpu {
            requirements.gpu = true;
        }
        requirements.trust_domain = self.trust_domain.clone();
        requirements
            .normalize()
            .map_err(|e| format!("placement requirements: {e}"))?;
        // Only mint a generation when a worker can actually take it: the
        // local run stays the answer otherwise.
        match self
            .plane
            .find_eligible(&self.organization, &self.trust_domain, &requirements)
        {
            Ok(None) => return Ok(faktor_orchestrator::placement::PlacementDecision::Local),
            Ok(Some(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
        let job_key = faktor_worker::JobKey::try_new(spec.job_key.clone())
            .map_err(|e| format!("placement job key: {e}"))?;
        let scheduled = self
            .plane
            .schedule_job(
                &self.organization,
                &self.trust_domain,
                &job_key,
                requirements,
                &spec.payload_digest,
                faktor_worker::default_requeue(),
                None,
            )
            .map_err(|e| e.to_string())?;
        match scheduled.lease {
            Some(lease) => Ok(faktor_orchestrator::placement::PlacementDecision::Remote {
                job_id: scheduled.job.job_id.to_string(),
                worker_id: lease.worker_id.to_string(),
                generation: scheduled.generation.as_u64(),
                lease_id: lease.lease_id.to_string(),
            }),
            // The CAS lost a race (the worker vanished between the
            // eligibility read and the accept): execute locally rather than
            // stranding the run.
            None => Ok(faktor_orchestrator::placement::PlacementDecision::Local),
        }
    }
}

/// The Wave 3 billing admission gate of the local daemon: it maps the
/// orchestrator's provider-neutral boundary hook onto the cloud entitlement
/// service for the daemon's configured tenant organization. The gate is
/// consulted at the three safe boundaries only (new task, new child spawn,
/// new provider attempt BEFORE dispatch); a refusal is a typed
/// `ExecError::AdmissionRefused` naming the exact limit and admits nothing.
struct BillingAdmissionGate {
    service: Arc<faktor_cloud::EntitlementService>,
    organization: faktor_cloud::OrganizationId,
}

impl BillingAdmissionGate {
    fn new(
        service: Arc<faktor_cloud::EntitlementService>,
        organization: faktor_cloud::OrganizationId,
    ) -> Self {
        Self {
            service,
            organization,
        }
    }
}

impl faktor_orchestrator::admission::AdmissionGate for BillingAdmissionGate {
    fn check(
        &self,
        request: &faktor_orchestrator::admission::AdmissionRequest,
    ) -> Result<(), faktor_orchestrator::admission::AdmissionRefusal> {
        use faktor_orchestrator::admission::AdmissionBoundary as Hook;
        let hook_boundary = request.boundary;
        let boundary = match hook_boundary {
            Hook::NewTask => faktor_cloud::AdmissionBoundary::NewTask,
            Hook::NewChildSpawn => faktor_cloud::AdmissionBoundary::NewChildSpawn,
            Hook::NewProviderAttempt => faktor_cloud::AdmissionBoundary::NewProviderAttempt,
        };
        let request = faktor_cloud::AdmissionRequest {
            boundary,
            task_id: request.task_id,
            provider: request.provider.clone(),
            observed: faktor_cloud::ObservedUsage {
                active_tasks: request.observed.active_tasks,
                children_of_task: request.observed.children_of_task,
                provider_attempts_of_task: request.observed.provider_attempts_of_task,
                estimated_provider_cost_micro: request.observed.estimated_provider_cost_micro,
            },
        };
        match self.service.check_admission(&self.organization, &request) {
            Ok(_) => Ok(()),
            Err(e) => Err(faktor_orchestrator::admission::AdmissionRefusal::new(
                hook_boundary,
                e.limit.clone(),
                e.to_string(),
            )),
        }
    }
}

/// The ACP backend over the REAL daemon: sessions, turns, and abort go
/// through the durable `SessionManager` and `AgentRuntime` (crash recovery,
/// journal, cancellation, and providers included). ACP wire/lifecycle
/// handling lives in `faktor-acp`; this seam only maps requests onto the
/// runtime, mirroring how the native server endpoints drive the daemon.
struct DaemonAcpBackend {
    session: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    /// The ONE product execution entry every ACP `session/prompt` goes
    /// through: an ordinary prompt becomes an in-session run through the
    /// daemon's TaskExecutor (default shadow mutation), identical to the
    /// Native and SDK prompt surfaces.
    prompts: Arc<faktor_server::native::PromptExecutionService>,
}

impl DaemonAcpBackend {
    fn new(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        prompts: Arc<faktor_server::native::PromptExecutionService>,
    ) -> Self {
        Self {
            session,
            agent,
            prompts,
        }
    }

    /// The session's provider: `params.provider` when given, else the ONLY
    /// registered provider instance (an ACP client without a provider
    /// preference binds the daemon's single provider deterministically).
    /// Zero or several providers refuse loudly instead of guessing.
    fn resolve_provider(&self, params: &Value) -> Result<String, String> {
        if let Some(p) = params.get("provider").and_then(Value::as_str) {
            return Ok(p.to_string());
        }
        let ids = self.agent.deps().providers.ids();
        match ids.len() {
            1 => Ok(ids.into_iter().next().expect("len 1")),
            0 => Err(
                "session/new: no providers are registered; configure one or pass params.provider"
                    .into(),
            ),
            _ => Err(format!(
                "session/new: multiple providers registered ({ids:?}); pass params.provider"
            )),
        }
    }
}

impl AcpBackend for DaemonAcpBackend {
    fn agent_info(&self) -> Value {
        let mut families: Vec<String> = self
            .agent
            .deps()
            .providers
            .all()
            .iter()
            .map(|p| p.id().to_string())
            .collect();
        families.sort();
        families.dedup();
        json!({
            "name": "Faktor",
            "version": faktor_core::VERSION,
            "providerFamilies": families,
        })
    }

    fn create_session(&self, params: &Value) -> Result<String, String> {
        let workspace = params
            .get("workspace")
            .and_then(Value::as_str)
            .unwrap_or("/");
        let title = params.get("title").and_then(Value::as_str).unwrap_or("acp");
        let model = params
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| self.agent.deps().model.clone());
        let provider = self.resolve_provider(params)?;
        let ws = self
            .session
            .create_workspace(workspace)
            .map_err(|e| e.message)?;
        let row = self
            .session
            .create_session(ws, title, &provider, &model)
            .map_err(|e| e.message)?;
        // Session lifecycle hook (audit): sessions are created here (the
        // session manager), not in the runtime — the daemon entry fires
        // SessionStart best-effort right after creation. Native server-side
        // session creation lives in crates/server, outside the CLI.
        self.agent
            .run_lifecycle_hook(faktor_hooks::HookEvent::SessionStart, row.id());
        Ok(row.id().to_string())
    }

    fn prompt(&self, session_id: &str, text: &str) -> Result<Value, String> {
        let sid = parse_session_id(session_id)?;
        if text.trim().is_empty() {
            return Err("prompt must not be empty".into());
        }
        if text.len() > faktor_session::MAX_PROMPT_BYTES {
            return Err(format!(
                "prompt of {} bytes exceeds the {} byte bound",
                text.len(),
                faktor_session::MAX_PROMPT_BYTES
            ));
        }
        // The ONE execution entry: the prompt becomes an in-session run
        // through the daemon's TaskExecutor (default shadow mutation). The
        // AcpBackend seam is synchronous (one serialized ACP request at a
        // time); the service call is async, so bridge sync → async on the
        // serve task via block_in_place (multi-threaded daemon runtime).
        let service = self.prompts.clone();
        let request = faktor_server::native::PromptRequest {
            prompt: text.to_string(),
            ..Default::default()
        };
        let receipt = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(service.prompt(sid, request))
        })
        .map_err(|e| e.to_string())?;
        let session = self.session.clone();
        if receipt.queued {
            // The prompt durably queued behind another actor's active turn;
            // the executor's own runner delivers it. Report the queued
            // acceptance, mirroring the previous backend behavior.
            let state = session
                .get_session(sid)
                .map_err(|e| e.message)?
                .ok_or_else(|| format!("session {sid}"))?
                .state()
                .map_err(|e| e.message)?;
            return Ok(json!({
                "status": "queued",
                "finalState": state,
            }));
        }
        // Accepted: wait for the turn machine to leave the mid-turn states
        // (the same durable wait the wire prompt path performs), then report
        // the machine's final state. Bounded by the configured turn budget
        // (fallback when the operator opted out with 0): a never-settling
        // machine yields a typed timeout instead of polling forever.
        let settle_deadline = {
            let budget = self.session.turn_budget_ms();
            if budget == 0 {
                TURN_SETTLE_FALLBACK
            } else {
                std::time::Duration::from_millis(budget)
            }
        };
        let state = tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current().block_on(await_turn_settled(
                &session,
                sid,
                settle_deadline,
            ))
        })?;
        Ok(json!({
            "status": "completed",
            "finalState": state,
        }))
    }

    fn abort(&self, session_id: &str) -> Result<(), String> {
        let sid = parse_session_id(session_id)?;
        self.agent.abort(sid).map(|_| ()).map_err(|e| e.message)
    }

    fn list_sessions(&self) -> Vec<String> {
        self.session
            .list_sessions(None)
            .map(|handles| handles.iter().map(|h| h.id().to_string()).collect())
            .unwrap_or_default()
    }
}

/// Session ids ride the ACP wire as plain decimal strings. Hostile ids
/// (non-numeric, zero, overflowing u64) are loud errors — never a panic.
fn parse_session_id(s: &str) -> Result<SessionId, String> {
    let raw: u64 = s.parse().map_err(|_| format!("invalid session id {s:?}"))?;
    if raw == 0 {
        return Err("invalid session id \"0\"".into());
    }
    Ok(SessionId::new(raw))
}

/// Poll interval of the ACP turn-settle wait.
const TURN_SETTLE_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Fallback ACP turn-settle deadline when the operator configured an
/// unbounded (`0`) wall-clock turn budget: the wait is still bounded.
const TURN_SETTLE_FALLBACK: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Wait for the durable turn machine to leave the mid-turn states, with an
/// explicit wall deadline. `read` returns the current durable state (or a
/// typed read error). A machine that never settles (dead executor, lost
/// wakeup, a stuck driver) exhausts the deadline and returns a TYPED timeout
/// naming the state last observed — the ACP bridge never polls forever and
/// never reports a silent success.
async fn await_settled<F>(
    mut read: F,
    deadline: std::time::Duration,
) -> Result<faktor_core::state::AgentState, String>
where
    F: FnMut() -> Result<faktor_core::state::AgentState, String>,
{
    let started = std::time::Instant::now();
    loop {
        let state = read()?;
        if !faktor_server::native::turn_machine_busy(state) {
            return Ok(state);
        }
        let elapsed = started.elapsed();
        if elapsed >= deadline {
            return Err(format!(
                "turn did not settle within {deadline:?} (still {state:?} after {elapsed:?})"
            ));
        }
        tokio::time::sleep(TURN_SETTLE_POLL.min(deadline - elapsed)).await;
    }
}

/// The session-backed [`await_settled`]: resolves the durable handle on every
/// poll (a recreated handle is never a stale in-memory state).
async fn await_turn_settled(
    session: &Arc<SessionManager>,
    sid: SessionId,
    deadline: std::time::Duration,
) -> Result<faktor_core::state::AgentState, String> {
    await_settled(
        || {
            let handle = match session.get_session(sid) {
                Ok(Some(h)) => h,
                Ok(None) => return Err(format!("session {sid}")),
                Err(e) => return Err(e.message),
            };
            handle.state().map_err(|e| e.message)
        },
        deadline,
    )
    .await
}

/// The daemon's session-owned terminal authority as the ACP
/// `faktor.terminal` seam: a thin adapter over
/// [`faktor_server::native::terminal_authority::TerminalRegistry`] — the same
/// `faktor-pty` + ownership-row authority the native terminal surface uses —
/// so the ACP server answers terminal methods only when the capability is
/// negotiated AND this authority is attached. Without it (or without
/// negotiation) every `terminal/*` method stays the official `-32601`.
struct DaemonTerminalAuthority {
    registry: Arc<faktor_server::native::terminal_authority::TerminalRegistry>,
}

impl DaemonTerminalAuthority {
    /// The authority is rooted at the SAME session manager the ACP backend
    /// drives, so a terminal can only be created for a real durable session
    /// and its row carries that session's durable task/operation identity.
    fn new(session: Arc<SessionManager>) -> Self {
        Self {
            registry: faktor_server::native::terminal_authority::TerminalRegistry::new(session),
        }
    }
}

/// Map the daemon registry error onto the ACP terminal error taxonomy
/// (invalid params vs internal refusal).
fn map_terminal_registry_error(
    error: faktor_server::native::terminal_authority::TerminalRegistryError,
) -> faktor_acp::TerminalError {
    use faktor_server::native::terminal_authority::TerminalRegistryError as RegistryError;
    match error {
        RegistryError::Invalid(message) => faktor_acp::TerminalError::Invalid(message),
        RegistryError::Refused(message) => faktor_acp::TerminalError::Refused(message),
        RegistryError::Unavailable(message) => faktor_acp::TerminalError::Unavailable(message),
    }
}

impl faktor_acp::TerminalAuthority for DaemonTerminalAuthority {
    fn create(
        &self,
        session_id: &str,
        spec: &faktor_acp::TerminalSpec,
    ) -> Result<Arc<dyn faktor_acp::TerminalHandle>, faktor_acp::TerminalError> {
        let request = faktor_server::native::terminal_authority::TerminalSpawnRequest {
            command: spec.command.clone(),
            args: spec.args.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone(),
            rows: spec.rows,
            cols: spec.cols,
        };
        let handle = self
            .registry
            .create(session_id, &request)
            .map_err(map_terminal_registry_error)?;
        Ok(Arc::new(DaemonTerminalHandle { handle }))
    }
}

/// One live daemon terminal behind the ACP [`faktor_acp::TerminalHandle`]
/// trait (delegation only; the authority owns the process).
struct DaemonTerminalHandle {
    handle: faktor_server::native::terminal_authority::TerminalHandle,
}

impl faktor_acp::TerminalHandle for DaemonTerminalHandle {
    fn terminal_id(&self) -> &str {
        self.handle.terminal_id()
    }
    fn pid(&self) -> u32 {
        self.handle.pid()
    }
    fn is_alive(&self) -> bool {
        self.handle.is_alive()
    }
    fn ownership_id(&self) -> &str {
        self.handle.ownership_id()
    }
    fn write(&self, bytes: &[u8]) -> Result<(), faktor_acp::TerminalError> {
        self.handle
            .write(bytes)
            .map_err(map_terminal_registry_error)
    }
    fn resize(&self, rows: u16, cols: u16) -> Result<(), faktor_acp::TerminalError> {
        self.handle
            .resize(rows, cols)
            .map_err(map_terminal_registry_error)
    }
    fn drain_output(&self) -> Vec<u8> {
        self.handle.drain_output()
    }
    fn kill(&self) -> Result<(), faktor_acp::TerminalError> {
        self.handle.kill().map_err(map_terminal_registry_error)
    }
}

/// Human-readable outcome of one doctor run. The shell wrapper prints the
/// lines and exits non-zero when `issues > 0`; tests call [`doctor_run`]
/// directly so a failing run never exits the test process.
struct DoctorReport {
    lines: Vec<String>,
    issues: usize,
}

// ------------------------------------------------------------------ updater
//
// The local parity surface of the signed updater: the SAME `faktor-updater`
// service the daemon's `/native/updater/*` routes drive, built from the
// `[updater]` section (disabled by default). Downloads ride the checked
// transport with the `[sandbox]` destination policy; `apply` runs the REAL
// doctor quick check as its post-swap health probe (a doctor failure rolls
// back to the previous artifact exactly).

/// The local health probe: the doctor-style quick check over the daemon's
/// data dir. `issues == 0` means healthy; anything else fails the probe and
/// the updater restores the previous pointer.
struct CliDoctorProbe {
    data_dir: PathBuf,
}

impl faktor_updater::HealthProbe for CliDoctorProbe {
    fn probe(
        &self,
        _layout: &faktor_updater::InstallLayout,
        pointer: &faktor_updater::InstallPointer,
    ) -> Result<(), faktor_updater::UpdateError> {
        let report = doctor_run(&self.data_dir, false);
        if report.issues == 0 {
            tracing::info!(
                "updater: doctor probe passed for {} ({})",
                pointer.version,
                pointer.digest
            );
            Ok(())
        } else {
            Err(faktor_updater::UpdateError::HealthFailed {
                detail: format!(
                    "doctor reports {} issue(s) after swapping in {} ({}): {}",
                    report.issues,
                    pointer.version,
                    pointer.digest,
                    report
                        .lines
                        .iter()
                        .find(|line| line.contains("FAIL"))
                        .cloned()
                        .unwrap_or_else(|| "see `faktor doctor`".into())
                ),
            })
        }
    }
}

/// Build the local updater from the `[updater]` section. Disabled sections
/// refuse loudly (never a silent no-op), and the checked transport carries
/// the daemon's `[sandbox]` destination policy.
fn build_local_updater(
    cfg: &config::UpdaterCfg,
    data_dir: &std::path::Path,
    policy: faktor_security::destination::DestinationPolicy,
    probe: Arc<dyn faktor_updater::HealthProbe>,
) -> Result<faktor_updater::Updater, String> {
    if !cfg.enabled {
        return Err(
            "updater: the [updater] section is disabled; enable it (and configure the operator \
             key allowlist) to use the updater"
                .into(),
        );
    }
    cfg.validate()?;
    let db = cfg
        .database_path(data_dir)?
        .ok_or("updater: enabled section resolved no database")?;
    let install_root = cfg
        .install_root_path(data_dir)?
        .ok_or("updater: enabled section resolved no install root")?;
    let store = Arc::new(
        faktor_updater::SqliteUpdaterStore::open(&db)
            .map_err(|e| format!("updater store {}: {e}", db.display()))?,
    );
    let fetcher = Arc::new(faktor_updater::CheckedHttpFetcher::new(
        faktor_provider::egress::CheckedHttpClient::try_with_policy(policy)
            .map_err(|e| format!("updater egress client: {e}"))?,
    ));
    let config = faktor_updater::UpdaterConfig {
        channel: cfg.channel()?,
        install_root,
        keys: cfg.trusted_keys()?,
        max_artifact_bytes: cfg.max_artifact_bytes_resolved(),
        clock_skew_ms: cfg.clock_skew_ms_resolved(),
        host_os: std::env::consts::OS.to_string(),
        host_arch: std::env::consts::ARCH.to_string(),
        local_version: faktor_core::VERSION.to_string(),
        allow_legacy_manifests_once: cfg.allow_legacy_manifests_once_resolved(),
    };
    faktor_updater::Updater::new(config, store, fetcher, probe).map_err(|e| e.to_string())
}

/// Read one manifest file under a bound (a manifest is tiny).
fn read_manifest_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
    const MAX_MANIFEST_FILE_BYTES: u64 = 1024 * 1024;
    let meta = std::fs::metadata(path).map_err(|e| format!("manifest {}: {e}", path.display()))?;
    if meta.len() > MAX_MANIFEST_FILE_BYTES {
        return Err(format!(
            "manifest {} is {} bytes (bound {MAX_MANIFEST_FILE_BYTES})",
            path.display(),
            meta.len()
        ));
    }
    std::fs::read(path).map_err(|e| format!("manifest {}: {e}", path.display()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `faktor updater <action>` — local parity with the control-plane routes.
async fn updater_command(action: UpdaterAction, data_dir: PathBuf, config_path: Option<PathBuf>) {
    let (config, _semantic) = match serve_config_and_semantic(config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("faktor updater: config error: {e}");
            std::process::exit(1);
        }
    };
    let probe: Arc<dyn faktor_updater::HealthProbe> = Arc::new(CliDoctorProbe {
        data_dir: data_dir.clone(),
    });
    let policy = match config.sandbox_policy() {
        Ok(policy) => policy
            .network
            .installed()
            .cloned()
            .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty),
        Err(e) => {
            eprintln!("faktor updater: sandbox config: {e}");
            std::process::exit(1);
        }
    };
    let updater = match build_local_updater(&config.updater, &data_dir, policy, probe) {
        Ok(updater) => Arc::new(updater),
        Err(e) => {
            eprintln!("faktor updater: {e}");
            std::process::exit(1);
        }
    };

    let rendered: Result<Value, String> = async {
        match action {
            UpdaterAction::Status => updater
                .status(now_ms())
                .map(|status| json!({ "status": status }))
                .map_err(|e| e.to_string()),
            UpdaterAction::Check {
                manifest,
                vscode,
                jetbrains,
            } => {
                let bytes = read_manifest_file(&PathBuf::from(&manifest))?;
                let running = updater
                    .running_components(None, None, vscode.as_deref(), jetbrains.as_deref())
                    .map_err(|e| e.to_string())?;
                updater
                    .check(&bytes, &running, now_ms())
                    .map(|outcome| json!({ "check": outcome }))
                    .map_err(|e| e.to_string())
            }
            UpdaterAction::Stage {
                manifest,
                key,
                vscode,
                jetbrains,
                release,
            } => {
                let bytes = read_manifest_file(&PathBuf::from(&manifest))?;
                let running = updater
                    .running_components(None, None, vscode.as_deref(), jetbrains.as_deref())
                    .map_err(|e| e.to_string())?;
                if release {
                    updater
                        .stage_release(&bytes, &running, key.as_deref(), now_ms())
                        .await
                        .map(|outcome| json!({ "stage_release": outcome }))
                        .map_err(|e| e.to_string())
                } else {
                    updater
                        .stage(&bytes, &running, key.as_deref(), now_ms())
                        .await
                        .map(|outcome| json!({ "stage": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Apply { confirm } => {
                if !confirm {
                    Err("apply requires --confirm: an update is never applied implicitly".into())
                } else {
                    updater
                        .apply(now_ms())
                        .map(|outcome| json!({ "apply": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Activate { confirm } => {
                if !confirm {
                    Err(
                        "activate requires --confirm: the pointer is never swapped implicitly"
                            .into(),
                    )
                } else {
                    updater
                        .activate_release(now_ms(), None)
                        .map(|outcome| json!({ "activate": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Finalize { digest, confirm } => {
                if !confirm {
                    Err(
                        "finalize requires --confirm: pass the digest the RUNNING process reported"
                            .into(),
                    )
                } else {
                    updater
                        .finalize_release(&digest, now_ms())
                        .map(|outcome| json!({ "finalize": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Abort { confirm } => {
                if !confirm {
                    Err("abort requires --confirm".into())
                } else {
                    updater
                        .abort_release(now_ms())
                        .map(|outcome| json!({ "abort": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Rollback { confirm } => {
                if !confirm {
                    Err("rollback requires --confirm".into())
                } else {
                    updater
                        .rollback(now_ms())
                        .map(|outcome| json!({ "apply": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Downgrade {
                manifest,
                key,
                vscode,
                jetbrains,
                confirm,
            } => {
                if !confirm {
                    Err(
                        "downgrade requires --confirm: the floor is never bypassed implicitly"
                            .into(),
                    )
                } else {
                    let bytes = read_manifest_file(&PathBuf::from(&manifest))?;
                    let running = updater
                        .running_components(None, None, vscode.as_deref(), jetbrains.as_deref())
                        .map_err(|e| e.to_string())?;
                    updater
                        .downgrade(
                            &bytes,
                            &running,
                            key.as_deref(),
                            Some("cli:local-operator"),
                            now_ms(),
                        )
                        .await
                        .map(|outcome| json!({ "apply": outcome }))
                        .map_err(|e| e.to_string())
                }
            }
            UpdaterAction::Recover => updater
                .recover(now_ms())
                .map(|outcomes| json!({ "recovered": outcomes }))
                .map_err(|e| e.to_string()),
        }
    }
    .await;

    match rendered {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or(value.to_string())
            );
        }
        Err(e) => {
            eprintln!("faktor updater: {e}");
            std::process::exit(1);
        }
    }
}

/// `faktor enterprise <action>` — local parity with the
/// `/native/enterprise/*` routes. The `[enterprise]` section must be
/// enabled; the local operator acts as an owner principal of the configured
/// organization (the daemon password is the local authority boundary, and
/// the same role matrix is applied). GC and deletion advances scan the
/// REAL session store and delete through the REAL guarded CAS.
/// Daemon config for the local commerce admin commands: an EXPLICIT
/// `--config` loads STRICTLY (a typo'd key is a startup error); without one
/// `faktor-plus.json` next to the data dir is used when present, and a
/// broken auto-discovered file falls back to defaults with a loud warning
/// (the same policy as `serve`/`acp`).
fn load_commerce_config(
    data_dir: &std::path::Path,
    config_path: Option<PathBuf>,
) -> Result<config::Config, String> {
    if let Some(path) = config_path {
        return config::Config::load_strict(&path);
    }
    let path = data_dir.join("faktor-plus.json");
    if !path.exists() {
        return Ok(config::Config::default());
    }
    match config::Config::load(&path) {
        Ok(config) => Ok(config),
        Err(e) => {
            tracing::error!("config error: {e}; using defaults");
            Ok(config::Config::default())
        }
    }
}

/// The local Faktor Acquire admin entry (`faktor commerce <action>`). Never
/// a model tool: no command here is registered in the agent's ToolRegistry,
/// and `login` hands the interactive flow to the headed profile browser.
async fn commerce_command(action: CommerceAction, data_dir: PathBuf, config_path: Option<PathBuf>) {
    let config = match load_commerce_config(&data_dir, config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("faktor commerce: config error: {e}");
            std::process::exit(1);
        }
    };
    let action = match action {
        CommerceAction::Doctor => tools_market::CommerceAdminAction::Doctor,
        CommerceAction::Status => tools_market::CommerceAdminAction::Status,
        CommerceAction::Login { source } => tools_market::CommerceAdminAction::Login(source),
        CommerceAction::Logout { source } => tools_market::CommerceAdminAction::Logout(source),
        CommerceAction::ClearCache => tools_market::CommerceAdminAction::ClearCache,
    };
    let browser: Arc<dyn tools_market::CommerceLoginBrowser> =
        Arc::new(tools_market::HeadedProfileBrowser);
    match tools_market::run_commerce_admin(action, &data_dir, &config.commerce, browser).await {
        Ok(output) => println!("{output}"),
        Err(e) => {
            eprintln!("faktor commerce: {e}");
            std::process::exit(1);
        }
    }
}

async fn enterprise_command(
    action: EnterpriseAction,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    let (config, _semantic) = match serve_config_and_semantic(config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("faktor enterprise: config error: {e}");
            std::process::exit(1);
        }
    };
    let rendered: Result<Value, String> = (|| {
        if !config.enterprise.enabled {
            return Err(
                "the [enterprise] section is disabled (set `enabled = true` and `organization`)"
                    .into(),
            );
        }
        let organization = config
            .enterprise
            .organization()?
            .ok_or("enterprise: enabled without an organization")?;
        let path = config
            .enterprise
            .enterprise_path(&data_dir)?
            .ok_or("enterprise: enabled without a database path")?;
        let store = Arc::new(
            faktor_cloud::SqliteControlPlaneStore::open(&path)
                .map_err(|e| format!("enterprise store {}: {e}", path.display()))?,
        ) as Arc<dyn faktor_cloud::EnterpriseStore>;
        let service = faktor_cloud::EnterpriseService::with_system_clock(store)
            .map_err(|e| format!("enterprise service: {e}"))?;
        let principal = faktor_cloud::Principal::user(
            faktor_cloud::UserId::try_new("local-operator").map_err(|e| e.to_string())?,
            organization.clone(),
            faktor_cloud::Role::Owner,
        );
        let needs_runtime = matches!(
            action,
            EnterpriseAction::Gc { .. }
                | EnterpriseAction::Deletion {
                    action: EnterpriseDeletionAction::Advance { .. }
                }
        );
        let runtime = if needs_runtime {
            let session_store = Arc::new(
                faktor_store::Store::open(data_dir.join("store"), false)
                    .map_err(|e| format!("session store: {e}"))?,
            );
            let cas = Arc::new(
                faktor_cas::Cas::open(data_dir.join("cas")).map_err(|e| format!("cas: {e}"))?,
            );
            Some(Arc::new(
                faktor_server::native::enterprise::RetentionRuntime::new(session_store, cas),
            ))
        } else {
            None
        };

        match action {
            EnterpriseAction::Status => service
                .status(&principal, &organization)
                .map(|status| json!({ "status": status }))
                .map_err(|e| e.to_string()),
            EnterpriseAction::Audit { after, limit } => service
                .audit_export(&principal, &organization, after, limit as usize)
                .map(|export| {
                    json!({
                        "items": export.events,
                        "nextCursor": export.next_cursor,
                        "headSeq": export.head_seq,
                    })
                })
                .map_err(|e| e.to_string()),
            EnterpriseAction::Gc { limit } => {
                let runtime = runtime
                    .as_ref()
                    .ok_or("enterprise: the gc runtime is unavailable (internal)")?;
                runtime
                    .gc(&service, &principal, limit as usize)
                    .map(|report| json!({ "report": report }))
                    .map_err(|e| e.to_string())
            }
            EnterpriseAction::Artifact { action } => match action {
                EnterpriseArtifactAction::List { cursor, limit } => service
                    .artifacts(&principal, &organization, cursor.as_deref(), limit as usize)
                    .map(|page| json!({ "items": page.items, "nextCursor": page.next_cursor }))
                    .map_err(|e| e.to_string()),
                EnterpriseArtifactAction::Register {
                    id,
                    kind,
                    digest,
                    size,
                    retention_class,
                    ttl_ms,
                    session,
                    task,
                    owner,
                } => {
                    let kind = faktor_cloud::ArtifactKind::parse(&kind)
                        .ok_or_else(|| format!("unknown artifact kind {kind:?}"))?;
                    let retention_class = faktor_cloud::RetentionClass::parse(&retention_class)
                        .ok_or_else(|| format!("unknown retention class {retention_class:?}"))?;
                    let new = faktor_cloud::NewArtifact {
                        id: faktor_cloud::ArtifactId::try_new(id).map_err(|e| e.to_string())?,
                        session,
                        task,
                        owner,
                        kind,
                        digest,
                        size,
                        retention_class,
                        ttl_ms,
                    };
                    service
                        .register_artifact(&principal, new)
                        .map(|record| json!({ "artifact": record }))
                        .map_err(|e| e.to_string())
                }
                EnterpriseArtifactAction::Eligible { id } => {
                    let id = faktor_cloud::ArtifactId::try_new(id).map_err(|e| e.to_string())?;
                    service
                        .mark_eligible(&principal, &id)
                        .map(|eligible| json!({ "eligible": eligible }))
                        .map_err(|e| e.to_string())
                }
            },
            EnterpriseAction::Deletion { action } => match action {
                EnterpriseDeletionAction::Start { scope, user } => {
                    let scope = match scope.as_str() {
                        "organization" => faktor_cloud::DeletionScope::Organization,
                        "account" => faktor_cloud::DeletionScope::Account {
                            user: faktor_cloud::UserId::try_new(
                                user.ok_or("deletion: an account scope requires --user")?,
                            )
                            .map_err(|e| e.to_string())?,
                        },
                        other => {
                            return Err(format!(
                                "deletion: scope {other:?} must be organization|account"
                            ));
                        }
                    };
                    service
                        .start_deletion(&principal, scope)
                        .map(|job| json!({ "job": job }))
                        .map_err(|e| e.to_string())
                }
                EnterpriseDeletionAction::Show { id } => {
                    let id = faktor_cloud::DeletionJobId::try_new(id).map_err(|e| e.to_string())?;
                    service
                        .deletion_job(&principal, &id)
                        .map(|job| json!({ "job": job }))
                        .map_err(|e| e.to_string())
                }
                EnterpriseDeletionAction::Advance { id } => {
                    let id = faktor_cloud::DeletionJobId::try_new(id).map_err(|e| e.to_string())?;
                    let runtime = runtime
                        .as_ref()
                        .ok_or("enterprise: the gc runtime is unavailable (internal)")?;
                    runtime
                        .advance(&service, &principal, &id)
                        .map(|job| json!({ "job": job }))
                        .map_err(|e| e.to_string())
                }
            },
            EnterpriseAction::Settings => service
                .settings(&principal, &organization)
                .map(|settings| json!({ "settings": settings }))
                .map_err(|e| e.to_string()),
            EnterpriseAction::Config => {
                let layers = config.enterprise.layers()?;
                faktor_cloud::resolve_layers(&layers)
                    .map(|effective| {
                        json!({
                            "digest": effective.digest,
                            "attestation": effective.attestation(),
                            "policies": effective.policies,
                            "preferences": effective.preferences,
                        })
                    })
                    .map_err(|e| e.to_string())
            }
        }
    })();

    match rendered {
        Ok(value) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&value).unwrap_or(value.to_string())
            );
        }
        Err(e) => {
            eprintln!("faktor enterprise: {e}");
            std::process::exit(1);
        }
    }
}

/// `faktor-cli doctor [--deep]`: plain mode opens with the bounded quick
/// path and reports quick checks; `--deep` additionally runs the full store
/// scan, the CAS blob verification, the global recovery-row scan, the
/// dangling-CAS-reference check (artifact rows + checkpoint after-blobs) and
/// the journal projection consistency checks. Issues are always SURFACED,
/// never repaired: the only automatic repair is stale-temp-file removal,
/// documented in [`remove_stale_temp_files`].
async fn doctor(data_dir: PathBuf, deep: bool, config_path: Option<PathBuf>) {
    let report = doctor_run_with_config(&data_dir, deep, config_path.as_deref());
    for line in &report.lines {
        println!("{line}");
    }
    if report.issues == 0 {
        println!("doctor: all checks passed");
    } else {
        println!("doctor: {} issue(s)", report.issues);
        std::process::exit(1);
    }
}

/// The config-free form used by the updater's post-swap health probe and by
/// the existing tests (no `[worker_plane]` audit).
fn doctor_run(data_dir: &std::path::Path, deep: bool) -> DoctorReport {
    doctor_run_with_config(data_dir, deep, None)
}

/// `doctor [--config <path>]`: the storage checks plus, when an explicit
/// config is named, the `[worker_plane]` deployment-boundary audit. The
/// boundary decision (including the `trusted_gateway` acknowledgement) is
/// surfaced line-by-line; a refused boundary is an ISSUE (the daemon would
/// refuse to start) and is never repaired.
fn doctor_run_with_config(
    data_dir: &std::path::Path,
    deep: bool,
    config_path: Option<&std::path::Path>,
) -> DoctorReport {
    let mut lines: Vec<String> = Vec::new();
    let mut issues = 0usize;
    doctor_worker_plane_line(config_path, &mut lines, &mut issues);
    doctor_sandbox_shell_line(config_path, &mut lines, &mut issues);
    match SessionManager::open_quick(data_dir.join("store"), data_dir.join("cas")) {
        Ok(session) => {
            lines.push("store: ok".into());
            let store = session.store();
            // Plain doctor uses the SAME bounded check the fast open runs
            // (audit 73): the full scan is a --deep concern.
            match store.diagnostics_quick() {
                Ok(d) => {
                    lines.push(serde_json::to_string_pretty(&d).unwrap());
                }
                Err(e) => {
                    lines.push(format!("store diagnostics failed: {e}"));
                    issues += 1;
                }
            }
            // Global unfinished-run count (plain doctor): the old report
            // only scanned session 1; every session counts now.
            match store.all_running_tool_rows() {
                Ok(runs) => {
                    lines.push(format!(
                        "unfinished tool runs across sessions: {}",
                        runs.len()
                    ));
                }
                Err(e) => {
                    lines.push(format!("running tool-run scan failed: {e}"));
                    issues += 1;
                }
            }
            // The ONE automatic repair doctor performs: stale temp/debris
            // removal only (documented). Everything deeper is surfaced, never
            // healed.
            let mut removed = Vec::new();
            remove_stale_temp_files(data_dir, &session, &mut removed);
            for path in removed {
                lines.push(format!("removed stale temp file: {path}"));
            }
            if deep {
                deep_doctor(&session, &mut lines, &mut issues);
            }
            // The additive index-embedding section (plain AND deep): the
            // typed build status of every published generation, read
            // read-only from the durable generation files the store's index
            // state names. Never opens a service, never builds, never
            // repairs.
            index_embedding_doctor(&store, &mut lines, &mut issues);
        }
        Err(e) => {
            lines.push(format!("store: FAILED ({e})"));
            issues += 1;
        }
    }
    // The additive commercial-database section (P1 durability): runs even when
    // the main store failed, so a corrupt commercial DB is always named.
    commercial_db_doctor(data_dir, deep, &mut lines, &mut issues);
    DoctorReport { lines, issues }
}

/// The `cloud-db` doctor section: every `*.db` under the data dir — top-level
/// AND nested (bounded depth, deterministic order, symlinks/hidden backup
/// trees skipped) — is probed READ-ONLY for pragma state, the
/// writer-recorded durability policy, integrity (quick in plain mode, full in
/// `--deep`), last verified rotating backup age and pre-migration
/// restore-point presence. Nothing is repaired; corruption is named (with its
/// data-root-relative path) and left alone.
///
/// The commercial databases are the LOCAL commercial deployment authority:
/// hosted deployments must move identity/auth/billing/credits/audit/retention
/// to a transactional service. The first line of the section says so.
fn commercial_db_doctor(
    data_dir: &std::path::Path,
    deep: bool,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    /// Bound on databases probed in one doctor run (the rest are NAMED as
    /// unprobed and fail the run — a silent truncation could hide one).
    const MAX_COMMERCIAL_DBS: usize = 32;
    /// Deepest directory level below the data root a `*.db` is discovered at
    /// (a file directly under the root is depth 0).
    const MAX_DB_DEPTH: usize = 3;
    /// Total integrity-issue lines printed across ALL probed databases (each
    /// probe keeps its own per-database bound; this is the section bound).
    const MAX_INTEGRITY_LINES: usize = 128;
    /// Directory names holding snapshots/blobs, never live authorities:
    /// probing every copy would consume the bounded probe budget and could
    /// hide a live database behind its own backups.
    const SKIP_DB_DIRS: &[&str] = &["backups", "commercial-backups", "cas"];
    /// Databases whose authority is the commercial control plane: a missing
    /// durability policy marker here is an issue, not information.
    const CLOUD_FAMILY: &[&str] = &[
        "control-plane.db",
        "billing.db",
        "enterprise.db",
        "scm.db",
        "workers.db",
        "update.db",
    ];
    let dbs = discover_commercial_dbs(data_dir, MAX_DB_DEPTH, SKIP_DB_DIRS);
    if dbs.is_empty() {
        return;
    }
    lines.push(format!(
        "cloud-db: {} local commercial database(s) (top-level and nested); LOCAL commercial deployment authority — hosted deployments must move identity/auth/billing/credits/audit/retention to a transactional service",
        dbs.len()
    ));
    if dbs.len() > MAX_COMMERCIAL_DBS {
        lines.push(format!(
            "cloud-db: {} database(s) beyond the {MAX_COMMERCIAL_DBS}-database probe budget were NOT probed",
            dbs.len() - MAX_COMMERCIAL_DBS
        ));
        *issues += 1;
    }
    let mut integrity_lines_printed = 0usize;
    let mut integrity_bound_reached = false;
    for (rel, path) in dbs.into_iter().take(MAX_COMMERCIAL_DBS) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let report = match faktor_cloud::durability::doctor_probe(&path, deep) {
            Ok(report) => report,
            Err(e) => {
                lines.push(format!("cloud-db {rel}: FAILED ({e})"));
                *issues += 1;
                continue;
            }
        };
        let policy_value = |key: &str| {
            report
                .policy
                .as_ref()
                .and_then(|rows| rows.iter().find(|(k, _)| k == key))
                .map(|(_, v)| v.as_str())
        };
        lines.push(format!(
            "cloud-db {rel}: journal_mode={} synchronous={} policy={} integrity={} tables={} rows={} digest={}",
            report.journal_mode,
            policy_value("synchronous").unwrap_or("unrecorded"),
            policy_value("policy_version").unwrap_or("unrecorded"),
            if report.integrity.is_empty() {
                "ok".to_string()
            } else {
                format!("{} issue(s)", report.integrity.len())
            },
            report.fingerprint.tables,
            report.fingerprint.rows,
            &report.fingerprint.digest[..16.min(report.fingerprint.digest.len())],
        ));
        match policy_value("synchronous") {
            Some("FULL") => {}
            Some(other) => {
                lines.push(format!(
                    "cloud-db {rel}: durability policy is {other}, expected FULL"
                ));
                *issues += 1;
            }
            None if CLOUD_FAMILY.contains(&name.as_str()) => {
                lines.push(format!(
                    "cloud-db {rel}: durability policy marker missing (last opened by pre-policy code?)"
                ));
                *issues += 1;
            }
            None => {}
        }
        for problem in &report.integrity {
            *issues += 1;
            if integrity_lines_printed < MAX_INTEGRITY_LINES {
                lines.push(format!("cloud-db {rel}: integrity: {problem}"));
                integrity_lines_printed += 1;
            } else if !integrity_bound_reached {
                lines.push(format!(
                    "cloud-db: further integrity issues are counted but not printed at the {MAX_INTEGRITY_LINES}-line output bound"
                ));
                integrity_bound_reached = true;
            }
        }
        match &report.last_backup {
            Some((backup, age)) => {
                lines.push(format!(
                    "cloud-db {rel}: last verified backup {age}s ago ({}) — {} rotating kept",
                    backup.display(),
                    report.backup_count
                ));
                if *age > faktor_cloud::durability::BACKUP_MAX_AGE_SECS {
                    lines.push(format!(
                        "cloud-db {rel}: last verified backup is STALE ({age}s old, threshold {}s)",
                        faktor_cloud::durability::BACKUP_MAX_AGE_SECS
                    ));
                    *issues += 1;
                }
            }
            None if policy_value("backup_policy") == Some("rotating") => {
                lines.push(format!(
                    "cloud-db {rel}: rotating backup policy recorded but no verified backup exists"
                ));
                *issues += 1;
            }
            None => lines.push(format!("cloud-db {rel}: no rotating backup")),
        }
        match &report.migration_restore_point {
            Some((point, age)) => lines.push(format!(
                "cloud-db {rel}: migration restore point {age}s old ({})",
                point.display()
            )),
            None => lines.push(format!(
                "cloud-db {rel}: migration restore point: none recorded"
            )),
        }
        // A restore point that is missing while required, unopenable, corrupt,
        // or whose name/content version claims disagree fails doctor LOUDLY,
        // naming this database (never silently trusted as a way back).
        if let Some(problem) = &report.migration_restore_point_issue {
            lines.push(format!(
                "cloud-db {rel}: migration restore point FAILED verification: {problem}"
            ));
            *issues += 1;
        }
    }
}

/// Every live `*.db` at or below `root`, in a deterministic order: discovery
/// is breadth-first with a bounded depth, each directory's entries are read
/// in name order, and the result is sorted by (depth, data-root-relative
/// path). Symlinks, hidden directories, the named snapshot/blob trees and
/// in-progress `*.tmp` files are never probed. `rel` is `/`-joined for
/// stable, platform-independent reporting.
fn discover_commercial_dbs(
    root: &std::path::Path,
    max_depth: usize,
    skip_dirs: &[&str],
) -> Vec<(String, std::path::PathBuf)> {
    let mut found: Vec<(usize, String, std::path::PathBuf)> = Vec::new();
    let mut frontier: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = frontier.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<std::fs::DirEntry> = entries.flatten().collect();
        children.sort_by_key(|entry| entry.file_name());
        for entry in children {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if name.starts_with('.') || skip_dirs.contains(&name.as_str()) {
                    continue;
                }
                if depth < max_depth {
                    frontier.push((entry.path(), depth + 1));
                }
                continue;
            }
            if !kind.is_file() || name.contains(".tmp") {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("db") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            found.push((depth, rel, path));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .map(|(_, rel, path)| (rel, path))
        .collect()
}

/// Bound on workspace generation directories probed by one doctor run (the
/// rest are NAMED as unprobed and fail the run — a silent truncation could
/// hide a degraded workspace).
const MAX_DOCTOR_INDEX_WORKSPACES: usize = 32;
/// Bound on one published generation file read for the embedding build
/// status. A doctor probe is an interactive check, not a load path: a larger
/// index is NAMED as unprobed (the daemon itself still serves it).
const MAX_DOCTOR_INDEX_GENERATION_BYTES: u64 = 64 * 1024 * 1024;
/// Bound of the persisted degraded reason rendered on one doctor line: a
/// hostile generation cannot inject newlines or balloon the report.
const MAX_DOCTOR_INDEX_REASON_CHARS: usize = 160;

/// The minimal slice of a published generation file the doctor decodes: the
/// envelope identity plus the persisted embedding build record. Every other
/// (potentially huge) member is skipped by serde without materialization, so
/// the probe's memory stays bounded no matter how large the published index
/// is.
#[derive(serde::Deserialize)]
struct DoctorGenerationStatus {
    format: u32,
    workspace: u64,
    generation: u64,
    data: DoctorGenerationData,
}

#[derive(serde::Deserialize)]
struct DoctorGenerationData {
    #[serde(default)]
    embeddings: DoctorGenerationEmbeddings,
}

#[derive(serde::Deserialize, Default)]
struct DoctorGenerationEmbeddings {
    #[serde(default)]
    format: u32,
    #[serde(default)]
    build: faktor_index::embedding::EmbeddingBuildRecord,
}

/// The `index embeddings` doctor section: the TYPED build outcome
/// (`unconfigured` / `complete` / `bounded` / `degraded` with the affected
/// chunk count and the bounded provider reason) of every workspace's
/// published generation — the same status
/// `IndexView::embedding_index(ws).build_status()` exposes to callers, read
/// here READ-ONLY from the durable generation file named by the store's
/// published index state. Doctor never builds, repairs or opens an
/// `IndexService`: it reports what is on disk (or names why it could not).
/// Bounded: at most [`MAX_DOCTOR_INDEX_WORKSPACES`] workspaces probed (the
/// excess is named and fails the run), at most one bounded read per
/// workspace, one output line with a bounded reason.
fn index_embedding_doctor(
    store: &faktor_store::Store,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(index_root) = store.path().parent().map(|p| p.join("index_data")) else {
        return;
    };
    let generations_root = index_root.join("generations");
    let Ok(entries) = std::fs::read_dir(&generations_root) else {
        lines.push("index embeddings: no persisted index generations".into());
        return;
    };
    let mut workspace_ids: Vec<u64> = Vec::new();
    let mut total = 0usize;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        // Symlinks/special entries are never followed (the index layout names
        // real numeric directories only).
        if !kind.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(raw) = name.to_str().and_then(|n| n.parse::<u64>().ok()) else {
            continue;
        };
        if raw == 0 {
            continue;
        }
        total += 1;
        if workspace_ids.len() < MAX_DOCTOR_INDEX_WORKSPACES {
            workspace_ids.push(raw);
        }
    }
    if total == 0 {
        lines.push("index embeddings: no persisted index generations".into());
        return;
    }
    workspace_ids.sort_unstable();
    lines.push(format!(
        "index embeddings: {total} workspace(s) with persisted index generations"
    ));
    if total > MAX_DOCTOR_INDEX_WORKSPACES {
        lines.push(format!(
            "index embeddings: {} workspace(s) beyond the {MAX_DOCTOR_INDEX_WORKSPACES}-workspace probe budget were NOT probed",
            total - MAX_DOCTOR_INDEX_WORKSPACES
        ));
        *issues += 1;
    }
    for raw in workspace_ids {
        let Ok(workspace) = faktor_core::id::WorkspaceId::try_from(raw) else {
            continue;
        };
        let row = match store.index_state_get(workspace) {
            Ok(row) => row,
            Err(e) => {
                lines.push(format!(
                    "index embeddings: workspace {raw}: durable index state read FAILED ({e})"
                ));
                *issues += 1;
                continue;
            }
        };
        let Some(row) = row else {
            lines.push(format!(
                "index embeddings: workspace {raw}: generation data exists with no durable index state"
            ));
            *issues += 1;
            continue;
        };
        let generation = match faktor_index::state::PersistedIndexState::parse(
            row.state_json.clone(),
            row.generation,
        ) {
            Ok(persisted) => match persisted.state {
                faktor_index::WorkspaceIndexState::Ready { generation }
                | faktor_index::WorkspaceIndexState::Dirty { generation } => generation,
                other => {
                    lines.push(format!(
                            "index embeddings: workspace {raw}: no published generation (machine state {})",
                            doctor_index_state_label(&other)
                        ));
                    continue;
                }
            },
            Err(e) => {
                lines.push(format!(
                    "index embeddings: workspace {raw}: durable index state CORRUPT ({e})"
                ));
                *issues += 1;
                continue;
            }
        };
        let path = generations_root
            .join(raw.to_string())
            .join(format!("gen-{generation}.json"));
        match read_published_embedding_status(&path, raw, generation) {
            Ok(Some(status)) => {
                lines.push(format_index_embedding_line(raw, generation, &status));
            }
            Ok(None) => {
                lines.push(format!(
                    "index embeddings: workspace {raw}: published generation {generation} is missing on disk (torn publish)"
                ));
                *issues += 1;
            }
            Err(e) => {
                lines.push(format!(
                    "index embeddings: workspace {raw}: generation {generation} unreadable: {e}"
                ));
                *issues += 1;
            }
        }
    }
}

/// The stable machine spelling of a persisted index state (doctor output).
fn doctor_index_state_label(state: &faktor_index::WorkspaceIndexState) -> &'static str {
    use faktor_index::WorkspaceIndexState as S;
    match state {
        S::NotStarted => "not_started",
        S::Building { .. } => "building",
        S::Dirty { .. } => "dirty",
        S::Ready { .. } => "ready",
        S::Failed { .. } => "failed",
    }
}

/// Read one published generation's typed embedding build status. `Ok(None)`
/// = the file is not present (the caller names it); every other failure is
/// an error string the caller surfaces as an issue. The decode mirrors the
/// service's own load checks (format tag, envelope identity) so doctor never
/// reports a status from bytes the daemon would refuse to serve.
fn read_published_embedding_status(
    path: &std::path::Path,
    raw: u64,
    generation: u64,
) -> Result<Option<faktor_index::embedding::EmbeddingBuildStatus>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("metadata: {e}")),
    };
    if meta.len() > MAX_DOCTOR_INDEX_GENERATION_BYTES {
        return Err(format!(
            "{} bytes exceeds the {MAX_DOCTOR_INDEX_GENERATION_BYTES}-byte doctor read bound",
            meta.len()
        ));
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("open: {e}")),
    };
    let decoded: DoctorGenerationStatus = serde_json::from_reader(std::io::BufReader::new(file))
        .map_err(|e| format!("decode: {e}"))?;
    if decoded.format != faktor_index::generation::GENERATION_FILE_FORMAT {
        return Err(format!(
            "generation file format {} unsupported (expected {})",
            decoded.format,
            faktor_index::generation::GENERATION_FILE_FORMAT
        ));
    }
    if decoded.workspace != raw || decoded.generation != generation {
        return Err(format!(
            "envelope claims workspace {}/generation {}, expected {raw}/{generation}",
            decoded.workspace, decoded.generation
        ));
    }
    // Mirror `EmbeddingIndex::sanitize`: another embedding format tag is
    // "no embeddings", never a fabricated status.
    if decoded.data.embeddings.format != faktor_index::embedding::EMBEDDING_FORMAT {
        return Ok(Some(
            faktor_index::embedding::EmbeddingBuildStatus::Unconfigured,
        ));
    }
    Ok(Some(decoded.data.embeddings.build.status()))
}

/// One line per workspace: the typed state name is exact (`unconfigured`,
/// `complete`, `bounded`, `degraded`); a degraded build additionally names
/// the affected chunk count and the bounded provider reason.
fn format_index_embedding_line(
    raw: u64,
    generation: u64,
    status: &faktor_index::embedding::EmbeddingBuildStatus,
) -> String {
    use faktor_index::embedding::EmbeddingBuildStatus as S;
    match status {
        S::Unconfigured => format!(
            "index embeddings: workspace {raw}: unconfigured (generation {generation}, no embedding source was configured at build time)"
        ),
        S::Complete { embedded, carried } => format!(
            "index embeddings: workspace {raw}: complete (generation {generation}, embedded={embedded}, carried={carried})"
        ),
        S::Bounded {
            embedded,
            carried,
            skipped,
        } => format!(
            "index embeddings: workspace {raw}: bounded (generation {generation}, embedded={embedded}, carried={carried}, skipped={skipped})"
        ),
        S::Degraded {
            reason,
            affected,
            embedded,
            carried,
            skipped,
        } => format!(
            "index embeddings: workspace {raw}: degraded affected={affected} (generation {generation}, embedded={embedded}, carried={carried}, skipped={skipped}) reason={}",
            doctor_index_reason_fragment(reason)
        ),
    }
}

/// Bounded one-line rendering of a persisted degraded reason: control
/// characters (including newlines a hostile generation may carry verbatim)
/// collapse to spaces so the report stays one line per workspace, and the
/// length is capped on a char boundary.
fn doctor_index_reason_fragment(reason: &str) -> String {
    let mut fragment: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_DOCTOR_INDEX_REASON_CHARS)
        .collect();
    if reason.chars().count() > MAX_DOCTOR_INDEX_REASON_CHARS {
        fragment.push('…');
    }
    fragment.trim().to_string()
}

/// The `[worker_plane]` deployment-boundary audit: records the resolved
/// exposure decision (including the `trusted_gateway` acknowledgement) in the
/// doctor report. A refused boundary is an ISSUE — the daemon refuses to
/// start on it — and is surfaced, never repaired. With no `--config` named
/// the check is skipped (the config-free doctor path is unchanged).
///
/// The config goes through the SAME load+validate path serve uses
/// (`serve_config_and_semantic`: structural strictness, `[semantic]` split,
/// semantic validation, provider/MCP/embedding/billing/worker bounds). A
/// config the daemon would refuse is reported as `state=refused` — never as
/// The doctor's shell-execution surface (item 10): reports the effective
/// `[sandbox]` shell contract with its honest strength label, so an
/// OS-isolated shell and a user-granted network-capable shell are never
/// presented as equivalent. Uses the explicit `--config` when given and the
/// daemon defaults otherwise; a config the daemon would refuse is reported
/// as an issue, never silently rendered as a healthy state.
fn doctor_sandbox_shell_line(
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let cfg = match config_path {
        Some(path) => match serve_config_and_semantic(Some(path.to_path_buf())) {
            Ok((config, _semantic)) => config,
            // The config refusal is already reported loudly by
            // doctor_worker_plane_line (the daemon would not start); do not
            // imply any shell state from a refused config.
            Err(_) => return,
        },
        None => config::Config::default(),
    };
    match cfg.sandbox_policy() {
        Ok(policy) => {
            let state = policy.shell_execution_state();
            lines.push(format!(
                "sandbox shell execution: {} (mode={}, network_guarantee={})",
                state.strength_label(),
                state.mode.as_tag(),
                state.network_guarantee.as_tag(),
            ));
        }
        Err(e) => {
            lines.push(format!("sandbox shell execution: FAILED {e}"));
            *issues += 1;
        }
    }
}

/// "enabled" — so the doctor cannot certify a daemon that would not start.
fn doctor_worker_plane_line(
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(path) = config_path else {
        return;
    };
    match serve_config_and_semantic(Some(path.to_path_buf())) {
        Ok((config, _semantic)) => {
            match config.worker_plane.resolve() {
                Ok(None) => {
                    lines.push(
                        "worker plane: disabled state=disabled enabled=false ([worker_plane] not enabled)"
                            .into(),
                    );
                }
                Ok(Some(bind_config)) => match bind_config.validate() {
                    Ok(exposure) => {
                        lines.push(format!(
                            "worker plane: enabled state=enabled enabled=true {}",
                            exposure.audit_line()
                        ));
                        if exposure.beyond_loopback {
                            lines.push(format!(
                                "worker plane: trusted_gateway acknowledgement recorded for {} (TLS/mTLS terminate at the gateway)",
                                exposure.bind
                            ));
                        }
                    }
                    Err(refusal) => {
                        lines.push(format!(
                            "worker plane: FAILED state=refused enabled=true code={} {refusal}",
                            refusal.code()
                        ));
                        *issues += 1;
                    }
                },
                Err(e) => {
                    // `resolve()` renders the typed deployment-boundary
                    // refusal with its stable machine code; any other failure
                    // is a plain config error (bad bind/auth shape). The
                    // operator sees which of the two refused the daemon.
                    let refusal_code = "worker_plane_boundary_refused";
                    if e.contains(refusal_code) {
                        lines.push(format!(
                            "worker plane: FAILED state=refused enabled=true code={refusal_code} {e}"
                        ));
                    } else {
                        lines.push(format!(
                            "worker plane: FAILED state=failed enabled=true {e}"
                        ));
                    }
                    *issues += 1;
                }
            }
            // The config already went through serve's full validation; the
            // boundary refusal above is the specific report when there is one.
        }
        Err(e) => {
            // The daemon REFUSES this config; the worker plane must never be
            // reported as enabled (the audit's trap: a config serve rejects
            // still rendered "enabled"). A worker-plane-specific refusal keeps
            // its stable machine code and its requested-enabled state; any
            // other config refusal reports the plane as NOT enabled (the
            // daemon would not open it).
            const REFUSAL_CODE: &str = "worker_plane_boundary_refused";
            if e.contains(REFUSAL_CODE) {
                lines.push(format!(
                    "worker plane: FAILED state=refused enabled=true code={REFUSAL_CODE} \
                     (the daemon refuses this config and would not start) {e}"
                ));
            } else {
                lines.push(format!(
                    "worker plane: FAILED state=refused enabled=false code=config_refused \
                     (the daemon refuses this config and would not open the worker plane) {e}"
                ));
            }
            *issues += 1;
        }
    }
}

/// `doctor --deep`: the full integrity scan, the CAS verification, the
/// cross-session recovery-row scan, the dangling-CAS-reference scan (which
/// also covers checkpoint after-blob refs), the journal projection checks and
/// the P0-97 audit invariants — dangling cost reservations, verification-
/// record/task consistency, active-turn recoverable owners, orphan children
/// (durable orchestrator rows) and the daemon-level process-ownership
/// report. None of these checks write to the store or the CAS: corruption is
/// listed and left alone (a second run must find the same issues).
fn deep_doctor(session: &Arc<SessionManager>, lines: &mut Vec<String>, issues: &mut usize) {
    let store = session.store();
    let add_issue = |line: String, lines: &mut Vec<String>, issues: &mut usize| {
        lines.push(line);
        *issues += 1;
    };
    // 1. Full store scan (the bounded quick check is NOT enough here).
    match store.deep_integrity_check() {
        Ok(found) if found.is_empty() => lines.push("deep store integrity scan: ok".into()),
        Ok(found) => {
            for issue in &found {
                lines.push(format!("deep store integrity scan: {issue}"));
            }
            *issues += found.len();
        }
        Err(e) => add_issue(
            format!("deep store integrity scan failed: {e}"),
            lines,
            issues,
        ),
    }
    // 2. CAS blob verification: every blob is decompressed and re-hashed.
    let cas = session.cas();
    let corrupted = cas.verify_integrity();
    if corrupted.is_empty() {
        lines.push("cas blob verification: ok".into());
    } else {
        let n = corrupted.len();
        for h in corrupted {
            lines.push(format!("cas blob corrupt: {}", h.to_hex()));
        }
        *issues += n;
    }
    // 3. Dangling CAS references (artifact rows + checkpoint after-blobs).
    //    Each referenced blob is STRICT-verified (decode + re-hash, P0-52):
    //    a corrupt-but-present blob is reported as corrupt, only a true
    //    absence is "missing". Path existence alone is never validity.
    match store.cas_hash_references() {
        Ok(refs) => {
            let mut dangling = Vec::new();
            let mut present = 0usize;
            for r in &refs {
                match faktor_core::hash::FileHash::from_hex(&r.hash) {
                    None => dangling.push(format!(
                        "{} row {} holds a malformed CAS hash {}",
                        r.source, r.row_id, r.hash
                    )),
                    Some(h) => match cas.verify_now(&h.to_hex()) {
                        Ok(_) => present += 1,
                        Err(faktor_cas::CasError::NotFound(_)) => dangling.push(format!(
                            "{} row {} references missing CAS blob {}",
                            r.source, r.row_id, r.hash
                        )),
                        Err(e) => dangling.push(format!(
                            "{} row {} references CORRUPT CAS blob {} ({e})",
                            r.source, r.row_id, r.hash
                        )),
                    },
                }
            }
            if dangling.is_empty() {
                lines.push(format!("cas references: {} hash(es) all verified", present));
            } else {
                let n = dangling.len();
                for d in dangling {
                    lines.push(format!("dangling cas reference: {d}"));
                }
                *issues += n;
            }
        }
        Err(e) => add_issue(format!("cas reference scan failed: {e}"), lines, issues),
    }
    // 4. Global recovery-row scan (INFORMATIONAL: a live daemon legitimately
    // has running rows; the report makes a crashed daemon's backlog visible).
    match store.all_running_tool_rows() {
        Ok(runs) => {
            lines.push(format!(
                "running tool runs across all sessions: {}",
                runs.len()
            ));
            for r in runs.iter().take(10) {
                lines.push(format!(
                    "  session {} op {} tool {} status {} effect {} started_ms {}",
                    r.session_id, r.op_id, r.tool, r.status, r.effect_status, r.started_ms
                ));
            }
        }
        Err(e) => add_issue(format!("running tool-run scan failed: {e}"), lines, issues),
    }
    match store.all_active_turns() {
        Ok(turns) => lines.push(format!(
            "active logical turns across all sessions: {}",
            turns.len()
        )),
        Err(e) => add_issue(format!("active turn scan failed: {e}"), lines, issues),
    }
    // 5. Journal projection consistency (gapless 1..=N per session).
    match store.journal_consistency_issues() {
        Ok(problems) if problems.is_empty() => lines.push("journal consistency: ok".into()),
        Ok(problems) => {
            for p in &problems {
                lines.push(format!("journal inconsistency: {p}"));
            }
            *issues += problems.len();
        }
        Err(e) => add_issue(
            format!("journal consistency scan failed: {e}"),
            lines,
            issues,
        ),
    }
    // 6. Durable cost-ledger invariant (P0-97): a reservation row is the
    //    ledger's handle onto its task envelope, so a row whose task row is
    //    gone can never settle or refund and its prediction silently leaves
    //    the cap math. Per-status counts are reported either way.
    match store.cost_reservation_invariants() {
        Ok(s) => {
            lines.push(format!(
                "cost reservations: {} (open {}, settled {}, refunded {}, uncertain {})",
                s.total, s.open, s.settled, s.refunded, s.uncertain
            ));
            if s.dangling.is_empty() {
                lines.push("dangling cost reservations: none".into());
            } else {
                for d in &s.dangling {
                    lines.push(format!(
                        "dangling cost reservation: reservation {} (session {} task {} op {}, status {}) references a task row that no longer exists (predicted {} micro)",
                        d.reservation_id, d.session_id, d.task_id, d.op_id, d.status, d.predicted_micro
                    ));
                }
                *issues += s.dangling.len();
            }
        }
        Err(e) => add_issue(format!("cost reservation scan failed: {e}"), lines, issues),
    }
    // 7. Verification-record consistency (P0-97, wave-16 invariant): records
    //    must reference existing task rows; a Passed record may certify the
    //    current revision only of a VerifiedComplete task; and a
    //    VerifiedComplete task must carry the Passed record its completion
    //    consumed. MUST fail loudly — completion proof is the row's only
    //    justification.
    match store.verification_record_invariants() {
        Ok(s) => {
            lines.push(format!(
                "verification records: {} record(s), {} completion-relevant task(s), {} VerifiedComplete task(s)",
                s.total_records, s.relevant_tasks, s.completed_tasks
            ));
            if s.issues.is_empty() {
                lines.push("verification consistency: ok".into());
            } else {
                for i in &s.issues {
                    lines.push(format!(
                        "verification inconsistency [{}]: {}",
                        i.kind, i.detail
                    ));
                }
                *issues += s.issues.len();
            }
        }
        Err(e) => add_issue(
            format!("verification consistency scan failed: {e}"),
            lines,
            issues,
        ),
    }
    // 8. Active-turn recoverable owners (P0-97, read-only semantics): a LIVE
    //    daemon legitimately owns active rows in memory, so the only durable
    //    question is whether a crashed daemon's recovery could own them — a
    //    prompt message row, a prompt-queue row, a journal event naming the
    //    turn op or a tool-run row must exist.
    match store.active_turn_ownership_invariants() {
        Ok(s) => {
            lines.push(format!(
                "active turns with recoverable owners: {} of {} (a live daemon owns the rest in memory)",
                s.recoverable, s.active_turns
            ));
            if !s.unrecoverable.is_empty() {
                for u in &s.unrecoverable {
                    lines.push(format!(
                        "active turn without recoverable owner: {}",
                        u.detail
                    ));
                }
                *issues += s.unrecoverable.len();
            }
        }
        Err(e) => add_issue(
            format!("active-turn ownership scan failed: {e}"),
            lines,
            issues,
        ),
    }
    // 9. Orphan children (P0-97/100, read-only): every durable child
    //    identity row must name an existing parent session, every executor
    //    registry row must name an existing child session under its own
    //    child_id, and a NON-TERMINAL child's worktree row + directory must
    //    still exist.
    let orphans =
        faktor_orchestrator::runtime::OrchestratorRuntime::orphan_children_scan(session.clone());
    lines.push(format!(
        "orphan children: {} child identity row(s), {} registry row(s) scanned",
        orphans.identity_rows, orphans.registry_rows
    ));
    if orphans.issues.is_empty() {
        lines.push("orphan children: none".into());
    } else {
        for i in &orphans.issues {
            lines.push(format!("orphan child: {i}"));
        }
        *issues += orphans.issues.len();
    }
    // 10. Orphan processes (P0-97): the session layer keeps NO durable
    //     process-ownership rows — ownership is the in-memory per-session
    //     registry, which dies with its owner session and is reported and
    //     cleared by crash recovery, and the daemon-level supervisor, whose
    //     Drop kills every still-live child when its last reference goes.
    //     Doctor is a separate process, so it reports the daemon-level live
    //     map of THIS process (informational: the zero-orphan guarantee is
    //     an in-process lifetime contract, not durable rows another process
    //     could audit).
    match ProcessSupervisor::try_shared() {
        Ok(shared) => {
            let alive = shared.alive();
            let session_owned = alive
                .iter()
                .filter(|c| matches!(c.owner, ProcessOwner::Session(_)))
                .count();
            lines.push(format!(
                "process ownership: 0 durable session-owned process row(s) (ownership is in-memory only); daemon-level live children in this process: {} alive ({} registered, {} session-owned)",
                alive.len(),
                shared.registered(),
                session_owned
            ));
        }
        Err(e) => lines.push(format!(
            "process ownership: standalone process authority unavailable in this process: {e}"
        )),
    }
}

/// Doctor's ONLY automatic repair (documented; audit 73/74): remove STALE
/// TEMP/debris files — a leftover rollback-journal sidecar next to a WAL
/// store, interrupted-backup temp files, and crashed CAS writer temps.
/// Age-guarded so a live writer's files are never touched. Everything else
/// (hash mismatches, journal inconsistencies, dangling references) is
/// surfaced as an issue and NEVER auto-repaired.
fn remove_stale_temp_files(
    data_dir: &std::path::Path,
    session: &Arc<SessionManager>,
    removed: &mut Vec<String>,
) {
    // (a) Rollback-journal debris: WAL-mode stores only create a -journal
    // sidecar during recovery; our open already replayed any real one, so a
    // -journal surviving next to a live -wal is stale debris. Without the
    // -wal marker the journal mode is unknowable — never delete.
    let store_dir = data_dir.join("store");
    let db_stem = store_dir.join("faktor-plus.db");
    let wal_live = db_stem.with_extension("db-wal").exists();
    let journal = db_stem.with_extension("db-journal");
    let hour = std::time::Duration::from_secs(3600);
    if wal_live && older_than(&journal, hour) && std::fs::remove_file(&journal).is_ok() {
        removed.push(journal.display().to_string());
    }
    // (b) Interrupted-backup temp files (crashed writers; see rotate_backup).
    let backups = data_dir.join("backups");
    if let Ok(files) = std::fs::read_dir(&backups) {
        for f in files.flatten() {
            let p = f.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.contains(".db.tmp-") && older_than(&p, hour) && std::fs::remove_file(&p).is_ok()
            {
                removed.push(p.display().to_string());
            }
        }
    }
    // (c) Crashed CAS writer temps (Cas tmp files are pid-uuid-tagged).
    let day = std::time::Duration::from_secs(24 * 3600);
    let cas_tmp = session.cas().root().join("tmp");
    if let Ok(files) = std::fs::read_dir(&cas_tmp) {
        for f in files.flatten() {
            let p = f.path();
            if older_than(&p, day) && std::fs::remove_file(&p).is_ok() {
                removed.push(p.display().to_string());
            }
        }
    }
}

async fn sessions(data_dir: PathBuf) {
    match SessionManager::open(data_dir.join("store"), data_dir.join("cas"), false) {
        Ok(session) => match session.list_sessions(None) {
            Ok(rows) => {
                for r in rows {
                    let state = r.state().map(|s| s.label()).unwrap_or("unknown");
                    let title = r.title().unwrap_or_default();
                    let provider = r.provider().unwrap_or_default();
                    let model = r.model().unwrap_or_default();
                    println!("{}  {title}  {provider}  {model}  [{state}]", r.id());
                }
            }
            Err(e) => eprintln!("error: {e}"),
        },
        Err(e) => eprintln!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::model::ModelCapabilities;
    use faktor_core::state::AgentState;
    use faktor_core::CancellationToken;
    use faktor_core::{Capability, CapabilitySet, OpId};
    use faktor_provider::testing::{sse_body, MockAction, MockServer};
    use faktor_provider::{
        ContentPart, FakeProvider, GenericAgentRequest, ProviderChunk, ProviderError,
        RequestMessage, RequestMeta, Role, ScriptedResponse, ToolSpec,
    };
    use faktor_terminal::{EnvSpec, ProcessOwner, SpawnConfig};
    use futures::StreamExt;
    use std::pin::Pin;

    /// Test-only default-allow transport: every mock endpoint below is a
    /// loopback server, and production construction injects the daemon's
    /// policy-checked transport instead.
    fn permissive_transport() -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::permissive())
    }

    /// Permission requester that never blocks on a UI (text-only turns never
    /// ask, but AgentDeps requires one deterministically).
    struct AlwaysAllow;
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
    fn test_agent(session: Arc<SessionManager>, registry: ProviderRegistry) -> Arc<AgentRuntime> {
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
            efficiency: Default::default(),
        };
        AgentRuntime::new(deps).unwrap()
    }

    /// The daemon's ONE execution facade over a test graph: the same
    /// construction the production entries use (one OrchestratorRuntime +
    /// TaskExecutor over the same session/agent, wrapped once in the
    /// PromptExecutionService every adapter calls).
    fn test_prompts(
        session: &Arc<SessionManager>,
        agent: &Arc<AgentRuntime>,
    ) -> Arc<faktor_server::native::PromptExecutionService> {
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
        let tasks =
            faktor_orchestrator::runtime::task_executor::TaskExecutor::new_owner_direct_for_test_harness(
                &orchestrator,
                session.clone(),
                agent.clone(),
            );
        faktor_server::native::PromptExecutionService::new(tasks, session.clone())
    }

    /// Every model request's system prompt, captured for wire assertions.
    struct CapturingProvider {
        inner: Arc<dyn Provider>,
        systems: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Provider for CapturingProvider {
        fn id(&self) -> &str {
            self.inner.id()
        }

        fn capabilities(&self, model: &str) -> ModelCapabilities {
            self.inner.capabilities(model)
        }

        fn supports_embeddings(&self, model: &str) -> bool {
            self.inner.supports_embeddings(model)
        }

        fn embed(
            &self,
            req: faktor_provider::EmbeddingRequest,
        ) -> Result<faktor_provider::EmbeddingResponse, ProviderError> {
            self.inner.embed(req)
        }

        fn stream(&self, req: GenericAgentRequest) -> faktor_provider::ProviderStream {
            self.systems.lock().unwrap().push(req.system.clone());
            self.inner.stream(req)
        }
    }

    /// A chat model that answers every stream identically (repeated evidence
    /// turns must never run out of script).
    struct AlwaysOk;

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

    /// The real daemon AgentDeps with an injected evidence provider (the
    /// semantic E2E needs `RepoEvidence` carrying a configured embedder).
    fn test_agent_with_evidence(
        session: Arc<SessionManager>,
        registry: ProviderRegistry,
        evidence: Arc<dyn faktor_agent::EvidenceProvider>,
    ) -> Arc<AgentRuntime> {
        let deps = AgentDeps {
            session: session.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: Arc::new(AlwaysAllow),
            evidence,
            tools: Arc::new(ToolRegistry::new()),
            cas: Some(session.cas()),
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            hooks: None,
            instructions_resolver: daemon_instructions_resolver(&session),
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
            efficiency: Default::default(),
        };
        AgentRuntime::new(deps).unwrap()
    }

    /// E2E (the required turn-level fence): an ORDINARY turn whose
    /// `[embeddings]` section selects the Ollama provider resolves through
    /// `Config::semantic_embedder` into the REAL `OllamaProvider` talking
    /// `/api/embed`, and the captured model request fuses the
    /// semantically-matched evidence (a file no lexical/symbol/exact search
    /// can find for "quantum zebra"). The SAME turn with no `[embeddings]`
    /// section stays neutral: zero embed calls and no evidence block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_ollama_embedder_fuses_semantic_evidence_into_an_ordinary_turn() {
        async fn captured_turn(configured: bool) -> (String, usize) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("repo");
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(
                root.join("src").join("ledger.rs"),
                "pub fn reconcile_accounts() -> u32 { 7 }\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src").join("parser.rs"),
                "pub fn parse_expr() -> u32 { 1 }\n",
            )
            .unwrap();
            let session =
                SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();

            // The Ollama mock: every `/api/embed` call answers keyword-axis
            // vectors in request order — query calls then the two-candidate
            // call (twice: once per concept).
            let server = MockServer::new();
            server.route(
                "POST",
                "/api/embed",
                MockAction::Sequence {
                    actions: vec![
                        MockAction::Respond {
                            status: 200,
                            body: r#"{"embeddings":[[1.0,0.0]]}"#.into(),
                        },
                        MockAction::Respond {
                            status: 200,
                            body: r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#.into(),
                        },
                        MockAction::Respond {
                            status: 200,
                            body: r#"{"embeddings":[[0.0,1.0]]}"#.into(),
                        },
                        MockAction::Respond {
                            status: 200,
                            body: r#"{"embeddings":[[1.0,0.0],[0.0,1.0]]}"#.into(),
                        },
                    ],
                },
            );
            let base = server.base_url().await;
            let ollama = faktor_ollama::OllamaProvider::new(
                faktor_ollama::OllamaConfig::new(Some(base)),
                permissive_transport(),
            );

            let systems = Arc::new(std::sync::Mutex::new(Vec::new()));
            let chat = Arc::new(CapturingProvider {
                inner: Arc::new(AlwaysOk),
                systems: systems.clone(),
            });
            let mut registry = ProviderRegistry::new();
            registry
                .try_register(faktor_provider::InstanceProvider::wrap(
                    chat as Arc<dyn Provider>,
                    "fake",
                ))
                .unwrap();
            registry.try_register(ollama as Arc<dyn Provider>).unwrap();

            let mut config = crate::config::Config::default();
            if configured {
                config.embeddings = Some(crate::config::EmbeddingCfg {
                    provider: "ollama".into(),
                    model: "nomic-embed-text".into(),
                    policy: crate::config::EmbeddingPolicy::BestEffort,
                });
            }
            let embedder = config
                .semantic_embedder(&registry, &faktor_core::retry::RetryPolicy::default())
                .unwrap();
            assert_eq!(
                embedder.is_some(),
                configured,
                "the section must resolve exactly when configured"
            );
            // Index generation BUILDS would drive the same provider with a
            // two-input chunk batch (`parse_expr` + `reconcile_accounts`),
            // racing the scripted one-input query calls below. Wrap the
            // resolved embedder so only query-time retrieval uses it: this
            // E2E is about semantic fusion into an ordinary turn, and the
            // script then has the deterministic one-input/one-candidate
            // cadence per concept. Persisted-vector retrieval is covered by
            // the search and evidence crate tests.
            struct QueryOnlyEmbedder(Arc<dyn faktor_search::Embedder>);
            impl faktor_search::Embedder for QueryOnlyEmbedder {
                fn embed(&self, texts: &[String]) -> Vec<Vec<f32>> {
                    faktor_search::Embedder::embed(self.0.as_ref(), texts)
                }
                fn try_embed(
                    &self,
                    texts: &[String],
                ) -> Result<Vec<Vec<f32>>, faktor_core::error::Error> {
                    faktor_search::Embedder::try_embed(self.0.as_ref(), texts)
                }
            }
            let embedder: Option<Arc<dyn faktor_search::Embedder>> = embedder
                .map(|e| Arc::new(QueryOnlyEmbedder(e)) as Arc<dyn faktor_search::Embedder>);

            let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
            let sid = session.create_session(ws, "idx", "fake", "m").unwrap().id();
            let evidence = Arc::new(RepoEvidence::new(session.clone(), embedder));
            let runtime = test_agent_with_evidence(session.clone(), registry, evidence);
            // Ordinary turns only: the runtime hosts its own IndexService
            // lazily, so the first turns legitimately serve while the
            // generation builds; later turns serve the Ready generation.
            // Poll by running ordinary turns (never by reaching into the
            // private service), bounded.
            let mut last_system = String::new();
            let attempts = if configured { 120 } else { 6 };
            for attempt in 0..attempts {
                runtime.run_turn(sid, "quantum zebra", &[]).await.unwrap();
                last_system = systems.lock().unwrap().last().cloned().unwrap_or_default();
                if configured && last_system.contains("## Retrieved evidence") {
                    break;
                }
                if attempt + 1 < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
            }
            (last_system, server.request_count())
        }

        let (system, embed_calls) = captured_turn(true).await;
        assert!(
            embed_calls >= 2,
            "the configured Ollama embedder must actually be called: {embed_calls}"
        );
        let evidence = system
            .split("## Retrieved evidence")
            .nth(1)
            .unwrap_or_default();
        assert!(
            evidence.contains("src/ledger.rs"),
            "the semantically-matched file must reach the captured request: {system}"
        );

        let (neutral, neutral_calls) = captured_turn(false).await;
        assert_eq!(neutral_calls, 0, "no embedder, no embed call");
        assert!(
            !neutral.contains("## Retrieved evidence"),
            "without an embedder no semantic evidence may be invented: {neutral}"
        );
    }

    /// The daemon's real write_file shape for ACP shadow tests: writes
    /// through the session's resolved workspace (the live shadow while a
    /// shadowed drive is running).
    fn acp_write_tool() -> faktor_agent::Tool {
        use faktor_agent::tool::RecoveryHint;
        use faktor_agent::{ToolOutcome, ToolRunCtx};
        use faktor_core::resource::ResourceClass;
        faktor_agent::Tool {
            name: "write_file".into(),
            description: "writes a real file".into(),
            input_schema: serde_json::json!({"type": "object"}),
            resource_class: ResourceClass::DiskWrite,
            capability: None,
            recovery_hint: RecoveryHint::WorkspaceWrite,
            path_args: vec!["path".into()],
            execute: Arc::new(move |ctx: ToolRunCtx, args| {
                Box::pin(async move {
                    let ws = ctx
                        .workspace
                        .ok_or_else(|| faktor_core::error::Error::internal("no workspace wired"))?;
                    let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                    let content = args
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default();
                    ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                        .map_err(|e| {
                            faktor_core::error::Error::internal(format!("write {path}: {e}"))
                        })?;
                    Ok(ToolOutcome {
                        text: format!("wrote {path}"),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        }
    }

    /// An ACP host over the FULL execution wiring: one session/agent, one
    /// shadow-carrying TaskExecutor, one PromptExecutionService the backend
    /// was constructed with. `owner` is the real checkout the ACP session
    /// points at.
    struct AcpShadowRig {
        dir: tempfile::TempDir,
        session: Arc<SessionManager>,
        #[allow(dead_code)]
        agent: Arc<AgentRuntime>,
        service: Arc<faktor_server::native::PromptExecutionService>,
        backend: DaemonAcpBackend,
    }

    fn acp_shadow_rig(scripts: Vec<ScriptedResponse>) -> AcpShadowRig {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                scripts,
            )))
            .unwrap();
        let mut tools = ToolRegistry::new();
        tools.register(acp_write_tool());
        let deps = AgentDeps {
            session: session.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: Arc::new(AlwaysAllow),
            evidence: Arc::new(faktor_agent::NoEvidence),
            tools: Arc::new(tools),
            cas: Some(session.cas()),
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            hooks: None,
            instructions_resolver: daemon_instructions_resolver(&session),
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: faktor_session::DurableBudgetLedger::new(session.clone()),
            model: "default".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are Faktor.".into(),
            clock: Arc::new(SystemClock),
            tool_call_mode: ToolCallMode::Native,
            tool_deadline_ms: 60_000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: faktor_agent::fallback_semantic_registry(),
            context_prior: None,
            efficiency: Default::default(),
        };
        let agent = AgentRuntime::new(deps).unwrap();
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
        let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
            session.clone(),
            dir.path().join("shadows"),
        )
        .unwrap();
        let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent.clone(),
            shadows,
        );
        let service = faktor_server::native::PromptExecutionService::new(tasks, session.clone());
        let backend = DaemonAcpBackend::new(session.clone(), agent.clone(), service.clone());
        AcpShadowRig {
            dir,
            session,
            agent,
            service,
            backend,
        }
    }

    /// A minimal REAL daemon over a temp data dir: one scripted provider
    /// registered under the instance id "fake" (the single registered
    /// instance, so the ACP session defaults resolve to it deterministically).
    fn acp_test_daemon(
        script: Vec<ScriptedResponse>,
    ) -> (tempfile::TempDir, Arc<SessionManager>, Arc<AgentRuntime>) {
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(FakeProvider::with_script(
                "fake",
                ModelCapabilities {
                    tools: true,
                    ..Default::default()
                },
                script,
            )))
            .unwrap();
        let agent = test_agent(session.clone(), registry);
        (dir, session, agent)
    }

    fn handle(session: &Arc<SessionManager>, sid: &str) -> faktor_session::SessionHandle {
        session
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn expand_home_and_relative() {
        assert_eq!(expand("."), PathBuf::from("."));
        let home = expand("~");
        assert_eq!(expand("~/x"), home.join("x"));
    }

    // ---- durable learning prior (audits 65-69/82) ----

    /// A REAL session manager with one workspace/session but an EMPTY
    /// durable learning corpus (no learning rows yet).
    fn empty_learning_session() -> (
        tempfile::TempDir,
        Arc<SessionManager>,
        faktor_core::id::SessionId,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ws = manager.create_workspace("/w").unwrap();
        let session = manager
            .create_session(ws, "learning", "fake", "m")
            .unwrap()
            .id();
        (dir, manager, session)
    }

    /// Mine ONE verified recovery into the session's durable ledger corpus
    /// through the REAL `SessionLearningStore` adapter — the same durable
    /// rows the runtime's mining hook appends. Returns the stored learning.
    fn mine_durable_corpus(
        manager: &Arc<SessionManager>,
        session: faktor_core::id::SessionId,
    ) -> faktor_learning::ProjectLearning {
        use faktor_core::id::VerificationRecordId;
        use faktor_learning::{
            ActionDescriptor, ActionFingerprint, EnvironmentFingerprint, EpisodeId,
            FailureDescriptor, FailureEpisode, FailureFingerprint, LearningService,
            LearningStore as _, ProjectScope, SessionLearningStore, TaskClass,
            DEFAULT_MEMORY_CAPACITY,
        };

        let handle = manager.get_session(session).unwrap().unwrap();
        let scope = ProjectScope::new(handle.row().unwrap().workspace_id, "w").unwrap();
        let episode = FailureEpisode::new(
            EpisodeId::new(1),
            TaskClass::new("bugfix").unwrap(),
            EnvironmentFingerprint::new(scope, "linux", "rustc", None).unwrap(),
            ActionFingerprint::of(
                &ActionDescriptor::new("edit", "src/lib.rs", Some("parse"), "attempt").unwrap(),
            ),
            FailureFingerprint::of(
                &FailureDescriptor::new("test_failure", None, "assertion failed").unwrap(),
            ),
        )
        .with_recovery_actions(vec![ActionFingerprint::of(
            &ActionDescriptor::new("edit", "src/lib.rs", Some("parse"), "guard").unwrap(),
        )])
        .unwrap()
        .verified(VerificationRecordId::new(11));
        let store = SessionLearningStore::open(handle, DEFAULT_MEMORY_CAPACITY).unwrap();
        let mut service = LearningService::new(store);
        service.mine_and_store(&[episode]).unwrap();
        service.store().all()[0].clone()
    }

    fn keyed_candidate(id: &str, keys: Vec<String>) -> faktor_context::ContextCandidate {
        faktor_context::ContextCandidate {
            id: id.into(),
            omission_keys: keys,
            ..Default::default()
        }
    }

    /// Audit 68 production wiring: the flag alone decides whether the
    /// durable session-manager-backed adapter is installed; with no learning
    /// rows the corpus index is empty and every candidate is neutral (byte
    /// parity with the flag-off path), keyed candidates included.
    #[test]
    fn failure_learning_prior_installs_only_on_flag_and_is_neutral_when_empty() {
        let (_dir, manager, _session) = empty_learning_session();
        assert!(
            daemon_context_prior(false, &manager).is_none(),
            "flag off => no prior handle"
        );
        let prior = daemon_context_prior(true, &manager).expect("flag on => learning adapter");
        for id in ["msg:0", "src/lib.rs", ""] {
            assert_eq!(
                prior.omission_risk(&keyed_candidate(id, Vec::new())),
                1.0,
                "empty corpus => neutral risk for {id:?}"
            );
        }
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec!["0".repeat(64)])),
            1.0,
            "empty corpus => a digest-shaped omission key is neutral too"
        );
    }

    /// The durable adapter over a REAL mined corpus: the candidate's
    /// `omission_keys` (never its render id) resolve to the learning's
    /// confidence-scaled risk (`1 + ppm/1e6`); empty/hostile/oversized keys
    /// can neither panic nor leave `[1, 2]`; and a fresh manager over the
    /// same data dir rebuilds the identical durable index (reopen-safe).
    #[test]
    fn durable_prior_resolves_omission_keys_ignores_render_ids_and_survives_reopen() {
        let (dir, manager, session) = empty_learning_session();
        let stored = mine_durable_corpus(&manager, session);
        assert_eq!(stored.confidence_ppm, 400_000, "one verified sample");
        let pattern = stored.pattern_digest().to_hex();
        let failure = stored.pattern.failure.digest().to_hex();

        let prior = daemon_context_prior(true, &manager).expect("flag on => durable prior");
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec![failure.clone()])),
            1.4
        );
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec![pattern.clone()])),
            1.4
        );
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec![failure.to_uppercase()],)),
            1.4,
            "hex parsing is case-insensitive"
        );
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", Vec::new())),
            1.0
        );
        // A render id that HAPPENS to be the digest is ignored: only
        // omission_keys are lookup keys.
        let id_only = faktor_context::ContextCandidate {
            id: failure.clone(),
            ..Default::default()
        };
        assert_eq!(
            prior.omission_risk(&id_only),
            1.0,
            "lookup must key omission_keys, never candidate.id"
        );
        // The max over several keys wins; hostile keys stay neutral.
        assert_eq!(
            prior.omission_risk(&keyed_candidate(
                "msg:0",
                vec!["0".repeat(64), failure.clone(), "src/lib.rs".into()],
            )),
            1.4
        );
        for key in [
            String::new(),
            "src/lib.rs".to_string(),
            "\0\u{1f600}".to_string(),
            "0".repeat(4096),
        ] {
            let risk = prior.omission_risk(&keyed_candidate("msg:0", vec![key.clone()]));
            assert!(
                risk.is_finite() && (1.0..=2.0).contains(&risk),
                "hostile key {key:?} produced {risk}"
            );
            assert_eq!(risk, 1.0, "{key:?}");
        }

        // Reopen-safety: a fresh manager over the same data dir rebuilds the
        // same durable index from the same ledger rows.
        drop(prior);
        drop(manager);
        let reopened =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let prior = daemon_context_prior(true, &reopened).expect("flag on");
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec![failure])),
            1.4
        );
    }

    /// The loop closes in-process (no restart): a prior built while the
    /// corpus was empty re-reads the durable ledger when the stamp advances
    /// and protects the freshly mined learning on the next lookup.
    #[test]
    fn durable_prior_refreshes_without_restart_when_the_corpus_is_mined() {
        let (_dir, manager, session) = empty_learning_session();
        let prior = daemon_context_prior(true, &manager).expect("flag on");
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec!["0".repeat(64)])),
            1.0
        );
        let stored = mine_durable_corpus(&manager, session);
        let failure = stored.pattern.failure.digest().to_hex();
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec![failure])),
            1.4,
            "the ledger stamp advanced; the index must re-read without a restart"
        );
    }

    /// Hostile/corrupt ledger rows are LOUD but non-fatal: the prior stays
    /// installed, keeps serving, and leaves the affected corpus neutral —
    /// including on a stamp-advancing refresh.
    #[test]
    fn corrupt_learning_row_is_loud_but_leaves_the_prior_neutral_and_total() {
        let (_dir, manager, session) = empty_learning_session();
        let handle = manager.get_session(session).unwrap().unwrap();
        handle
            .ledger_learning_record(faktor_session::LEARNING_RECORD_LEARNING, "not json")
            .unwrap();
        let prior =
            daemon_context_prior(true, &manager).expect("flag on => prior despite corrupt row");
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec!["a".repeat(64)])),
            1.0
        );
        // Another corrupt row advances the stamp: the refresh is still
        // non-fatal and still neutral.
        handle
            .ledger_learning_record(faktor_session::LEARNING_RECORD_LEARNING, "still not json")
            .unwrap();
        assert_eq!(
            prior.omission_risk(&keyed_candidate("msg:0", vec!["a".repeat(64)])),
            1.0
        );
    }

    /// The full production selection loop (audits 65-69/82): a durable mined
    /// corpus -> the wire planner's `learning:<digest>` evidence exposes the
    /// digest through `omission_keys` -> the CLI's durable prior protects it
    /// enough to flip the single evidence slot, and the exact same corpus
    /// survives a manager reopen.
    #[test]
    fn durable_prior_flips_planner_selection_and_survives_reopen() {
        use faktor_context::assembler::Evidence;
        use faktor_context::budget::ContextBudget;
        use faktor_context::information::FailurePrior;
        use faktor_context::ledger::TaskLedger;
        use faktor_context::wire_plan::WirePlan;
        use faktor_context::TokenCache;

        fn plan(
            evidence: &[Evidence],
            budget: &ContextBudget,
            ledger: &TaskLedger,
            cache: &TokenCache,
            prior: Option<&(dyn FailurePrior + Send + Sync)>,
        ) -> WirePlan {
            faktor_agent::wire_plan::plan_wire_turn_with_prior(
                "You are a test agent.\n",
                "",
                &[],
                "",
                ledger,
                "",
                &[],
                evidence,
                budget,
                "gpt-5",
                cache,
                prior,
            )
            .unwrap()
        }

        let (dir, manager, session) = empty_learning_session();
        let stored = mine_durable_corpus(&manager, session);
        let failure = stored.pattern.failure.digest().to_hex();

        // Two equal-priced evidence blocks compete for exactly one slot;
        // only the learning-sourced one carries an omission key.
        let evidence = vec![
            Evidence {
                path: format!("learning:{failure}"),
                snippet: "x".repeat(96),
                score: 0.5,
            },
            Evidence {
                path: "src/b.rs".into(),
                snippet: "x".repeat(400),
                score: 0.6,
            },
        ];
        let budget = ContextBudget {
            system: 130,
            tools: 0,
            working: 0,
            retrieved: 0,
            recent: 0,
            output_reserve: 0,
            safety: 0,
        };
        let ledger = TaskLedger::default();
        let cache = TokenCache::new();

        let off = plan(&evidence, &budget, &ledger, &cache, None);
        assert!(
            off.system.contains("### src/b.rs") && !off.system.contains("### learning:"),
            "baseline: the 0.6 block wins the single slot; system={}",
            off.system
        );

        let prior = daemon_context_prior(true, &manager).expect("flag on");
        let on = plan(&evidence, &budget, &ledger, &cache, Some(prior.as_ref()));
        assert!(
            on.system.contains(&format!("### learning:{failure}")),
            "the mined learning's omission risk must protect its candidate"
        );
        assert!(!on.system.contains("### src/b.rs"));
        assert_eq!(
            off.cacheable_prefix().unwrap(),
            on.cacheable_prefix().unwrap(),
            "the prior may only change volatile selection"
        );

        // Durable across reopen: the same corpus rebuilds the same prior.
        drop(prior);
        drop(manager);
        let reopened =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let prior = daemon_context_prior(true, &reopened).expect("flag on");
        let again = plan(&evidence, &budget, &ledger, &cache, Some(prior.as_ref()));
        assert!(again.system.contains(&format!("### learning:{failure}")));
    }

    /// The parsed `[efficiency]` switches thread 1:1 onto the agent-side
    /// flags (`failure_learning` included); the additive default is all-off.
    #[test]
    fn efficiency_flags_mirror_every_parsed_switch() {
        let all = config::EfficiencyCfg {
            failure_learning: true,
            ccr: true,
            typed_handoff: true,
            semantic_context: true,
            rework_routing: true,
        };
        assert_eq!(
            efficiency_flags(&all),
            faktor_agent::EfficiencyFlags {
                failure_learning: true,
                ccr: true,
                typed_handoff: true,
                semantic_context: true,
                rework_routing: true,
            }
        );
        assert!(efficiency_flags(&all).failure_learning);
        assert_eq!(
            efficiency_flags(&config::EfficiencyCfg::default()),
            faktor_agent::EfficiencyFlags::default(),
            "additive default: every flag off"
        );
    }

    /// Every enabled-section database open goes through
    /// `enabled_section_db_path`: a resolver `None` is a TYPED config error
    /// (never an `unreachable!` startup abort, never a silently created
    /// fallback database). The disabled section is the only resolver path
    /// that returns `None`, for all four sections.
    #[test]
    fn enabled_sections_resolve_typed_errors_never_panic() {
        for section in ["cloud control-plane", "cloud scm", "billing", "workers"] {
            let err = enabled_section_db_path(section, None)
                .expect_err("an enabled section with no resolved path must refuse");
            assert!(err.starts_with(&format!("{section} config:")), "{err}");
            assert!(err.contains("resolved no database path"), "{err}");
            let path = std::path::PathBuf::from("/tmp/faktor-db");
            assert_eq!(
                enabled_section_db_path(section, Some(path.clone())).unwrap(),
                path
            );
        }
        // The resolvers' None branch is exactly the disabled section for
        // every one of the four configs the call sites guard.
        let dir = std::path::Path::new("/data");
        assert_eq!(
            config::CloudCfg::default().control_plane_path(dir).unwrap(),
            None
        );
        assert_eq!(config::CloudCfg::default().scm_path(dir).unwrap(), None);
        assert_eq!(
            config::BillingCfg::default().billing_path(dir).unwrap(),
            None
        );
        assert_eq!(
            config::WorkersCfg::default().workers_path(dir).unwrap(),
            None
        );
    }

    /// The ACP turn-settle wait is BOUNDED: a machine that never leaves a
    /// busy state returns a typed timeout on a short deadline instead of
    /// polling forever; a settling machine returns its terminal state; a
    /// durable read error propagates (never a silent success).
    #[tokio::test]
    async fn acp_turn_settle_wait_is_bounded_and_typed() {
        let t0 = std::time::Instant::now();
        let err = await_settled(
            || Ok(faktor_core::state::AgentState::Streaming),
            std::time::Duration::from_millis(40),
        )
        .await
        .expect_err("a never-settling turn must time out");
        assert!(err.contains("did not settle"), "{err}");
        assert!(err.contains("Streaming"), "{err}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "the wait must be bounded by the deadline, not by the poll"
        );

        let mut calls = 0u32;
        let state = await_settled(
            || {
                calls += 1;
                Ok(if calls < 3 {
                    faktor_core::state::AgentState::ExecutingTool
                } else {
                    faktor_core::state::AgentState::ReadyForNextTurn
                })
            },
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(state, faktor_core::state::AgentState::ReadyForNextTurn);

        let err = await_settled(
            || Err("durable read failed".to_string()),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("durable read failed"), "{err}");
    }

    #[test]
    fn serve_config_is_strict_only_for_an_explicit_path() {
        // Audit 31: without --config, defaults (nothing can fail startup);
        // with an explicit --config, parse+validation failures are startup
        // errors — never a silent fallback to defaults.
        assert_eq!(
            serve_config(None).unwrap().model,
            config::Config::default().model,
            "no --config stays lenient"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.json");
        std::fs::write(&path, "{not json").unwrap();
        let e = serve_config(Some(path.clone()))
            .expect_err("an explicit broken config must fail startup");
        assert!(e.contains("serve.json"), "{e}");
        // Unknown fields and duplicate provider ids fail strict too.
        std::fs::write(&path, r#"{"model": "m", "surprise": 1}"#).unwrap();
        assert!(serve_config(Some(path.clone())).is_err());
        std::fs::write(
            &path,
            r#"{"providers": [
                {"kind": "ollama", "id": "twice", "base_url": null},
                {"kind": "open_ai", "id": "twice", "base_url": "http://x"}
            ]}"#,
        )
        .unwrap();
        let e = serve_config(Some(path.clone())).expect_err("duplicate ids fail strict");
        assert!(e.contains("twice"), "{e}");
        // A healthy explicit config still loads.
        std::fs::write(
            &path,
            r#"{"config_version": 1, "model": "m", "providers": [
                {"kind": "ollama", "id": "o", "base_url": null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(serve_config(Some(path)).unwrap().model, "m");
    }

    #[test]
    fn semantic_section_is_additive_strict_and_empty_by_default() {
        // The `[semantic]` section (audits 48-54/58/79) is additive: absent
        // keeps the fallback-only registry; a present section parses under
        // the SAME strict document rules as every other config key.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.json");
        let (_cfg, semantic) = serve_config_and_semantic(None).unwrap();
        assert_eq!(semantic, graph::SemanticCfg::default());
        std::fs::write(&path, r#"{"config_version": 1, "model": "m"}"#).unwrap();
        let (cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
        assert_eq!(cfg.model, "m");
        assert_eq!(semantic, graph::SemanticCfg::default(), "empty default");
        std::fs::write(
            &path,
            r#"{"config_version": 1, "model": "m", "semantic": {
                "max_payload_bytes": 2048, "max_entity_refs": 64
            }}"#,
        )
        .unwrap();
        let (cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
        assert_eq!(cfg.model, "m");
        assert_eq!(semantic.max_payload_bytes, Some(2048));
        assert_eq!(semantic.max_entity_refs, Some(64));
        let supervisor =
            ProcessSupervisor::new(Arc::new(faktor_cas::Cas::new(dir.path().join("cas"))));
        let transport: Arc<dyn HttpTransport> = permissive_transport();
        let registry = graph::semantic_registry(&semantic, &supervisor, &transport).unwrap();
        assert!(
            registry.providers().is_empty(),
            "caps-only section registers no provider"
        );
        // A configured external provider builds through the daemon's own
        // authorities (registration itself spawns nothing).
        std::fs::write(
            &path,
            r#"{"config_version": 1, "model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "local-proc", "command": "/bin/true", "timeout_ms": 1000},
                {"kind": "http", "id": "remote", "endpoint": "http://provider.example/semantic", "timeout_ms": 1000}
            ]}}"#,
        )
        .unwrap();
        let (_cfg, semantic) = serve_config_and_semantic(Some(path.clone())).unwrap();
        let registry = graph::semantic_registry(&semantic, &supervisor, &transport).unwrap();
        let ids: Vec<String> = registry
            .providers()
            .iter()
            .map(|p| p.id().as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["local-proc".to_string(), "remote".to_string()]);
        // Duplicate provider ids are refused at strict load.
        std::fs::write(
            &path,
            r#"{"model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "dup", "command": "/bin/true", "timeout_ms": 1000},
                {"kind": "http", "id": "dup", "endpoint": "http://x", "timeout_ms": 1000}
            ]}}"#,
        )
        .unwrap();
        let e = serve_config_and_semantic(Some(path.clone())).expect_err("duplicate ids");
        assert!(e.contains("[semantic]") && e.contains("dup"), "{e}");
        // Hostile provider values (zero timeout, non-http endpoint) refuse
        // startup here, before any graph authority exists.
        for bad in [
            r#"{"model": "m", "semantic": {"providers": [
                {"kind": "process", "id": "p", "command": "/bin/true", "timeout_ms": 0}
            ]}}"#,
            r#"{"model": "m", "semantic": {"providers": [
                {"kind": "http", "id": "h", "endpoint": "ftp://x", "timeout_ms": 1000}
            ]}}"#,
        ] {
            std::fs::write(&path, bad).unwrap();
            let e = serve_config_and_semantic(Some(path.clone())).expect_err("hostile provider");
            assert!(e.contains("[semantic]"), "{e}");
        }
        // Strict inside the section: a typo'd key fails startup.
        std::fs::write(&path, r#"{"model": "m", "semantic": {"surprise": true}}"#).unwrap();
        let e = serve_config_and_semantic(Some(path.clone())).expect_err("strict section");
        assert!(e.contains("[semantic]"), "{e}");
        // Unknown fields OUTSIDE the section still fail exactly as before.
        std::fs::write(&path, r#"{"model": "m", "surprise": 1}"#).unwrap();
        assert!(serve_config_and_semantic(Some(path)).is_err());
    }

    #[test]
    fn semantic_registry_arc_is_the_one_authority_for_agent_and_server() {
        // Audit 83 lock: the graph builds the semantic registry EXACTLY
        // once; the SAME Arc flows to AgentDeps and to ServerDeps (the
        // exact serve_impl assembly), so the native introspection surface
        // can never report a parallel registry.
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let session = SessionManager::open(data.join("store"), data.join("cas"), true).unwrap();
        let supervisor = ProcessSupervisor::new(session.cas());
        let semantic = graph::SemanticCfg {
            providers: vec![faktor_semantic::SemanticProviderConfig::Process {
                id: faktor_semantic::SemanticProviderId::parse("graph-proc").unwrap(),
                command: "/bin/true".to_string(),
                args: vec![],
                timeout_ms: 1_000,
            }],
            ..Default::default()
        };
        let graph = build_daemon_core(
            &data,
            session,
            supervisor,
            config::Config::default(),
            vec![],
            None,
            semantic,
        )
        .expect("daemon core builds with a configured semantic provider");
        assert_eq!(graph.semantic.providers().len(), 1);
        assert!(
            Arc::ptr_eq(graph.agent.semantic_registry(), &graph.semantic),
            "the agent must hold the graph's semantic Arc"
        );
        // The ServerDeps assembly mirrors serve_impl exactly.
        let mut deps = ServerDeps::new_with(
            graph.session.clone(),
            graph.agent.clone(),
            graph.permissions.clone(),
            graph.orchestrator.clone(),
            graph.tasks.clone(),
            graph.budgets.clone(),
        );
        deps = deps.with_semantic_registry(graph.semantic.clone());
        let served = deps.semantic.as_ref().expect("semantic wired");
        assert!(
            Arc::ptr_eq(served, graph.agent.semantic_registry()),
            "the native surface (deps.semantic) and the agent must share ONE Arc"
        );
        assert_eq!(served.providers()[0].id().as_str(), "graph-proc");
    }

    #[test]
    fn daemon_instructions_resolver_uses_durable_session_roots_only() {
        // P0-32: the daemon resolver must never invent a root — loading
        // repository rules at the wrong root would silently misapply them.
        // Resolution goes through the SessionManager workspace table only:
        // an unknown/rootless workspace is an Empty set (documented), and a
        // durable workspace root serves its AGENTS.md with live-epoch
        // semantics (rewrite -> new epoch/content; pinned old epoch serves
        // the cached old tree).
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let resolver = daemon_instructions_resolver(&session);
        // Unknown workspace id: Empty, never an error, never the CWD.
        assert!(resolver.resolve(999, None).unwrap().is_empty());
        // A durable workspace root serves the tree at that root.
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "always: durable daemon rules\n").unwrap();
        let ws = session.create_workspace(repo.to_str().unwrap()).unwrap();
        let loaded = resolver.resolve(ws.raw(), None).unwrap();
        assert!(loaded
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("durable daemon rules")));
        let e1 = loaded.epoch().unwrap();
        // A rewrite moves the epoch and the served content; the pinned old
        // epoch still sees the old content through the cache.
        std::fs::write(repo.join("AGENTS.md"), "always: rewritten daemon rules\n").unwrap();
        let v2 = resolver.resolve(ws.raw(), None).unwrap();
        assert_ne!(v2.epoch().unwrap(), e1);
        assert!(v2
            .active_for("x", &[])
            .iter()
            .any(|i| i.content.contains("rewritten daemon rules")));
        let pinned = resolver.resolve(ws.raw(), Some(e1)).unwrap();
        assert!(
            pinned
                .active_for("x", &[])
                .iter()
                .any(|i| i.content.contains("durable daemon rules")),
            "a pinned old epoch must still serve the old tree"
        );
    }

    #[test]
    fn daemon_startup_recovery_requeues_stale_running_jobs_only() {
        // Audit P0-5/26 production wiring: the startup sweep re-queues the
        // stale Running row a dead executor left behind and NEVER touches a
        // terminal row.
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap(),
        );
        let ws = manager.create_workspace("/w").unwrap();
        let sid = manager
            .create_session(ws, "recover", "fake", "m")
            .unwrap()
            .id();
        let handle = manager.get_session(sid).unwrap().unwrap();
        let task = handle.task_id().unwrap();
        let op = manager.try_next_op_id().unwrap().raw();
        let check = |id: &str| faktor_session::VerificationAttemptCheck {
            check_id: id.into(),
            command: format!("make {id}"),
            inline: None,
        };
        let job = |id: &str| faktor_session::VerificationJobInput {
            check_id: id.into(),
            kind: "test".into(),
            command: format!("make {id}"),
            program: "make".into(),
            args: vec![id.into()],
            spec_json: "{}".into(),
            budget_ms: 10_000,
        };
        handle
            .begin_verification_attempt(
                task.raw(),
                1,
                op,
                "/w",
                &[],
                &[check("make_test"), check("make_check")],
                &[job("make_test"), job("make_check")],
            )
            .unwrap();
        // One executor died mid-check (Running); one finished (Passed).
        handle
            .claim_verification_job(
                task.raw(),
                "make_test",
                op,
                manager.try_next_op_id().unwrap().raw(),
            )
            .unwrap();
        handle
            .claim_verification_job(
                task.raw(),
                "make_check",
                op,
                manager.try_next_op_id().unwrap().raw(),
            )
            .unwrap();
        handle
            .resolve_verification_job(
                task.raw(),
                "make_check",
                op,
                faktor_session::VerificationJobState::Passed,
                None,
                Some("{}".into()),
            )
            .unwrap();
        recover_verification_jobs_at_startup(&manager);
        let jobs = handle.verification_attempt_jobs(task.raw(), op).unwrap();
        let stale = jobs
            .iter()
            .find(|j| j.check_id == "make_test")
            .expect("stale row");
        assert_eq!(stale.state, faktor_session::VerificationJobState::Queued);
        assert!(stale.note.as_deref().unwrap().contains("restart"));
        let done = jobs
            .iter()
            .find(|j| j.check_id == "make_check")
            .expect("terminal row");
        assert_eq!(done.state, faktor_session::VerificationJobState::Passed);
    }

    /// F10 (adversarial): a store read failure during the startup
    /// verification sweep is TYPED and counted — never a silent `continue`
    /// that looks like "nothing to recover"; the durable rows stay for the
    /// next boot.
    #[test]
    fn startup_verification_recovery_reports_unreadable_rows() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap(),
        );
        let ws = manager.create_workspace("/w").unwrap();
        let _sid = manager
            .create_session(ws, "recover", "fake", "m")
            .unwrap()
            .id();
        // Healthy baseline: a typed zero, no failure.
        let healthy = recover_verification_jobs_at_startup(&manager);
        assert_eq!(healthy, VerificationRecoverySummary::default());
        assert!(!healthy.scan_failed);
        // Corrupt the durable verification store under the live manager:
        // the sweep must report the unreadable session instead of skipping.
        {
            let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db"))
                .unwrap();
            conn.execute_batch("DROP TABLE verification_job").unwrap();
        }
        let summary = recover_verification_jobs_at_startup(&manager);
        assert_eq!(summary.unreadable, 1, "{summary:?}");
        assert!(!summary.scan_failed, "{summary:?}");
        assert_eq!(summary.requeued, 0, "{summary:?}");
    }

    /// F10 (adversarial): the startup queue-head sweep never classifies an
    /// unreadable/unopenable session as "already drained" — it counts it
    /// typed (an issue the operator can see) instead of silently skipping.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn startup_queue_recovery_reports_unopenable_rows_instead_of_silence() {
        let dir = tempfile::tempdir().unwrap();
        let graph = build_daemon(dir.path(), None).unwrap();
        let ws = graph.session.create_workspace("/w").unwrap();
        let handle = graph.session.create_session(ws, "t", "fake", "m").unwrap();
        let op = graph.session.try_next_op_id().unwrap();
        graph
            .session
            .store()
            .enqueue_prompt(handle.id(), op, "queued", &[], None, None, None, 1)
            .unwrap();
        let healthy = recover_pending_queues_at_startup(&graph);
        assert_eq!(
            (healthy.candidates, healthy.runnable, healthy.unreadable),
            (1, 1, 0),
            "{healthy:?}"
        );
        assert!(!healthy.scan_failed, "{healthy:?}");
        // A durable queue row whose session does not exist (foreign row,
        // written with FK enforcement off): the sweep lists it as a candidate
        // but cannot open it — that is reported, never silently "drained".
        {
            let conn = rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db"))
                .unwrap();
            conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
            conn.execute(
                "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, status, requested_at) \
                 VALUES (999999, 1, 999999, 'foreign', 'pending', 1)",
                [],
            )
            .unwrap();
            conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
        }
        let summary = recover_pending_queues_at_startup(&graph);
        assert_eq!(summary.candidates, 2, "{summary:?}");
        assert_eq!(
            summary.unreadable, 1,
            "the unopenable foreign row must be reported, not silent: {summary:?}"
        );
        assert!(!summary.scan_failed, "{summary:?}");
    }
    #[test]
    fn daemon_instructions_resolver_reads_the_live_shadow_of_a_shadowed_workspace() {
        // P0-48: while exactly ONE session of the workspace carries a live
        // shadow row, the resolver's rules come from the SHADOW root (the
        // drive reads the instruction environment of the world it mutates);
        // with no live shadow (or after retirement) the stored workspace
        // root serves byte-identically; ambiguity is a loud degrade.
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let resolver = daemon_instructions_resolver(&session);
        let repo = dir.path().join("repo");
        let shadow = dir.path().join("shadows").join("1").join("sh-x");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&shadow).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "always: user-checkout rules\n").unwrap();
        std::fs::write(shadow.join("AGENTS.md"), "always: shadow-world rules\n").unwrap();
        let ws = session.create_workspace(repo.to_str().unwrap()).unwrap();
        let handle = session.create_session(ws, "t", "fake", "m").unwrap();
        let loaded = resolver.resolve(ws.raw(), None).unwrap();
        assert!(loaded
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("user-checkout rules")));
        // Begin a durable shadow (the exact row the ShadowRoots service
        // writes): the workspace's rules now resolve from the shadow.
        let row = faktor_session::ShadowRow {
            session_id: handle.id().raw(),
            shadow_id: "sh-x".into(),
            base_root: repo.to_str().unwrap().into(),
            root: shadow.to_str().unwrap().into(),
            state: faktor_session::ShadowRowState::Active,
            base_entries: 1,
            base_bytes: 1,
            created_ms: session.now_ms(),
        };
        session.put_shadow_row(handle.id(), &row).unwrap();
        let shadowed = resolver.resolve(ws.raw(), None).unwrap();
        assert!(
            shadowed
                .active_for("anything", &[])
                .iter()
                .any(|i| i.content.contains("shadow-world rules")),
            "the live shadow re-points instruction loading"
        );
        // Retire the shadow: the stored root is authoritative again.
        let mut retired = row;
        retired.state = faktor_session::ShadowRowState::Integrated;
        session.put_shadow_row(handle.id(), &retired).unwrap();
        let back = resolver.resolve(ws.raw(), None).unwrap();
        assert!(back
            .active_for("anything", &[])
            .iter()
            .any(|i| i.content.contains("user-checkout rules")));
        // Two live shadows on one workspace: ambiguous — loud degrade to the
        // stored root (never a guessed root).
        let other = session.create_session(ws, "other", "fake", "m").unwrap();
        let mut other_row = faktor_session::ShadowRow {
            session_id: other.id().raw(),
            shadow_id: "sh-y".into(),
            base_root: repo.to_str().unwrap().into(),
            root: shadow.to_str().unwrap().into(),
            state: faktor_session::ShadowRowState::Active,
            base_entries: 1,
            base_bytes: 1,
            created_ms: session.now_ms(),
        };
        session.put_shadow_row(other.id(), &other_row).unwrap();
        let mut revived = retired;
        revived.state = faktor_session::ShadowRowState::IntegrationBlocked;
        session.put_shadow_row(handle.id(), &revived).unwrap();
        other_row.state = faktor_session::ShadowRowState::Active;
        let loaded = resolver.resolve(ws.raw(), None).unwrap();
        assert!(
            loaded
                .active_for("anything", &[])
                .iter()
                .any(|i| i.content.contains("user-checkout rules")),
            "ambiguous shadows never hijack the stored root"
        );
    }

    #[test]
    fn parse_hooks_env_skips_malformed_entries_and_bounds_the_count() {
        // Hostile/malformed env: garbage entries are skipped, never a panic
        // and never a registered hook. Bounded to MAX_ENV_HOOKS entries.
        let raw = "  ; no_colon_here ; pre_tool: ; nope:true; bogus_event:/bin/true";
        let specs = parse_hooks_env(raw);
        assert!(specs.is_empty(), "every entry is malformed: {specs:?}");

        let ok = "task_complete:/bin/true";
        let specs = parse_hooks_env(ok);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "env-0");
        assert_eq!(specs[0].events, vec![faktor_hooks::HookEvent::TaskComplete]);
        assert_eq!(specs[0].command, "/bin/true");
        assert!(specs[0].args.is_empty());
        assert!(specs[0].env_allowlist, "env allowlist is mandatory");
        assert_eq!(
            specs[0].failure_policy,
            faktor_hooks::FailurePolicy::FailClosed,
            "FailClosed is the default failure policy"
        );

        // Malformed entries between valid ones are skipped; ids stay
        // contiguous over the parsed (not the raw) positions.
        let mixed = "pre_tool:/bin/a arg1;garbage;post_tool:/bin/b";
        let specs = parse_hooks_env(mixed);
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "env-0");
        assert_eq!(specs[0].events, vec![faktor_hooks::HookEvent::PreTool]);
        assert_eq!(specs[0].args, vec!["arg1".to_string()]);
        assert_eq!(specs[1].id, "env-1");
        assert_eq!(specs[1].events, vec![faktor_hooks::HookEvent::PostTool]);

        // A hostile env with more than MAX_ENV_HOOKS entries is capped.
        let many = (0..100)
            .map(|i| format!("pre_tool:/bin/true {i}"))
            .collect::<Vec<_>>()
            .join(";");
        let specs = parse_hooks_env(&many);
        assert_eq!(specs.len(), MAX_ENV_HOOKS);
    }

    #[test]
    fn parse_hooks_env_recognizes_every_hook_event_name() {
        // Every snake_case event name the runtime can fire must parse, so an
        // env entry never silently drops a supported event.
        let names = [
            ("session_start", faktor_hooks::HookEvent::SessionStart),
            ("session_resume", faktor_hooks::HookEvent::SessionResume),
            ("task_start", faktor_hooks::HookEvent::TaskStart),
            ("pre_model", faktor_hooks::HookEvent::PreModel),
            ("post_model", faktor_hooks::HookEvent::PostModel),
            ("pre_tool", faktor_hooks::HookEvent::PreTool),
            ("post_tool", faktor_hooks::HookEvent::PostTool),
            ("tool_error", faktor_hooks::HookEvent::ToolError),
            ("pre_edit", faktor_hooks::HookEvent::PreEdit),
            ("post_edit", faktor_hooks::HookEvent::PostEdit),
            ("pre_commit", faktor_hooks::HookEvent::PreCommit),
            ("subagent_start", faktor_hooks::HookEvent::SubagentStart),
            ("subagent_stop", faktor_hooks::HookEvent::SubagentStop),
            ("agent_error", faktor_hooks::HookEvent::AgentError),
            ("agent_stop", faktor_hooks::HookEvent::AgentStop),
            ("task_complete", faktor_hooks::HookEvent::TaskComplete),
            ("session_end", faktor_hooks::HookEvent::SessionEnd),
        ];
        for (name, event) in names {
            let specs = parse_hooks_env(&format!("{name}:/bin/true"));
            assert_eq!(specs.len(), 1, "event {name} must parse");
            assert_eq!(specs[0].events, vec![event], "event {name}");
        }
    }

    #[test]
    fn parsed_env_hook_registers_and_fires_on_a_real_registry() {
        // End-to-end shape of the daemon wiring: a parsed spec registers on
        // a real HookRegistry and the hook FIRES for its event (the audit
        // log gains the record). /bin/echo exists on the CI platforms.
        let specs = parse_hooks_env("post_tool:/bin/echo hook-fired");
        assert_eq!(specs.len(), 1);
        let registry = Arc::new(faktor_hooks::HookRegistry::with_supervisor(
            ProcessSupervisor::try_shared().expect("standalone supervisor"),
            faktor_core::CapabilitySet::ALL,
        ));
        for spec in specs {
            registry.register(spec).unwrap();
        }
        let verdict = registry.run(
            faktor_hooks::HookEvent::PostTool,
            &faktor_hooks::HookInput {
                event: faktor_hooks::HookEvent::PostTool,
                ..Default::default()
            },
        );
        assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
        let audit = registry.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].hook_id, "env-0");
        assert_eq!(audit[0].event, faktor_hooks::HookEvent::PostTool);
    }

    #[test]
    fn agent_info_names_faktor_and_lists_registered_provider_families() {
        let (_dir, session, agent) = acp_test_daemon(vec![]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session, agent, prompts);
        let info = backend.agent_info();
        assert_eq!(info["name"], "Faktor");
        assert_eq!(info["version"], faktor_core::VERSION);
        assert_eq!(
            info["providerFamilies"],
            json!(["fake"]),
            "the scripted provider's family id must surface"
        );
    }

    #[test]
    fn create_session_applies_defaults_and_list_sessions_sees_it() {
        let (_dir, session, agent) = acp_test_daemon(vec![]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session.clone(), agent, prompts);

        // Defaults: workspace "/", title "acp", daemon model, single
        // registered provider.
        let sid = backend.create_session(&json!({})).unwrap();
        assert!(!sid.is_empty());
        let row = handle(&session, &sid).row().unwrap();
        assert_eq!(row.title, "acp");
        assert_eq!(row.provider, "fake");
        assert_eq!(row.model, "default");
        assert!(backend.list_sessions().contains(&sid));

        // Explicit params override every default and stay listable.
        let sid2 = backend
            .create_session(&json!({
                "workspace": "/elsewhere",
                "title": "zed-import",
                "provider": "fake",
                "model": "m",
            }))
            .unwrap();
        let row2 = handle(&session, &sid2).row().unwrap();
        assert_eq!(row2.title, "zed-import");
        assert_eq!(row2.model, "m");
        assert_eq!(
            session
                .store()
                .workspace_root(row2.workspace_id)
                .unwrap()
                .as_deref(),
            Some("/elsewhere")
        );
        assert!(backend.list_sessions().contains(&sid2));
        assert_eq!(backend.list_sessions().len(), 2);

        // No registered provider: refusing loudly beats a phantom session.
        let (_d2, session3, agent3) = {
            let dir2 = tempfile::tempdir().unwrap();
            let s = SessionManager::open(dir2.path().join("store"), dir2.path().join("cas"), true)
                .unwrap();
            let a = test_agent(s.clone(), ProviderRegistry::new());
            (dir2, s, a)
        };
        let backend3 = DaemonAcpBackend::new(
            session3.clone(),
            agent3.clone(),
            test_prompts(&session3, &agent3),
        );
        let err = backend3.create_session(&json!({})).unwrap_err();
        assert!(err.contains("no providers"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_runs_a_real_turn_and_reports_the_final_state() {
        let (_dir, session, agent) = acp_test_daemon(vec![
            ScriptedResponse::Text("pong".into()),
            ScriptedResponse::End,
        ]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session, agent, prompts);
        let sid = backend.create_session(&json!({})).unwrap();

        let result = backend.prompt(&sid, "ping").unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["finalState"], "ready_for_next_turn");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_on_unknown_session_errors() {
        let (_dir, session, agent) = acp_test_daemon(vec![
            ScriptedResponse::Text("pong".into()),
            ScriptedResponse::End,
        ]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session, agent, prompts);
        let unknown = format!("{}", u64::MAX - 1);
        let err = backend.prompt(&unknown, "hi").unwrap_err();
        assert!(err.contains(&unknown), "{err}");
        assert!(backend.abort(&unknown).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_cancels_a_live_turn_and_keeps_the_session_usable() {
        let (_dir, session, agent) = acp_test_daemon(vec![
            ScriptedResponse::Text("pong".into()),
            ScriptedResponse::End,
        ]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session.clone(), agent.clone(), prompts);

        // A: the turn is durably ACTIVE (Preparing, live op registered, never
        // driven) — abort must land the machine ReadyForNextTurn.
        let sid_a = backend
            .create_session(&json!({ "title": "mid-flight" }))
            .unwrap();
        agent
            .submit(SessionId::new(sid_a.parse().unwrap()), "stop me", &[])
            .unwrap();
        assert!(handle(&session, &sid_a).state().unwrap().is_active());
        assert!(backend.abort(&sid_a).is_ok());
        assert_eq!(
            handle(&session, &sid_a).state().unwrap(),
            AgentState::ReadyForNextTurn
        );

        // B: an idle abort (Stop cancels the turn, never the session) keeps
        // the session promptable — a real turn still completes afterwards.
        let sid_b = backend.create_session(&json!({})).unwrap();
        assert!(backend.abort(&sid_b).is_ok());
        let result = backend.prompt(&sid_b, "ping").unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["finalState"], "ready_for_next_turn");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hostile_inputs_are_errs_never_panics() {
        let (_dir, session, agent) = acp_test_daemon(vec![
            ScriptedResponse::Text("pong".into()),
            ScriptedResponse::End,
        ]);
        let prompts = test_prompts(&session, &agent);
        let backend = DaemonAcpBackend::new(session.clone(), agent, prompts);
        let sid = backend.create_session(&json!({})).unwrap();

        // Empty and whitespace prompts are refused before the runtime.
        assert!(backend.prompt(&sid, "").is_err());
        assert!(backend.prompt(&sid, "   \n\t ").is_err());

        // Oversized prompts are refused by the daemon bound (never a panic,
        // never an unbounded journal write).
        let huge = "x".repeat(5 * 1024 * 1024);
        let err = backend.prompt(&sid, &huge).unwrap_err();
        assert!(
            err.contains("exceeds") && err.contains("bound"),
            "oversized prompt must be refused, got: {err}"
        );

        // Hostile session ids: non-numeric, zero, overflowing u64.
        for bad in ["abc", "0", "18446744073709551616", "-1", ""] {
            assert!(backend.prompt(bad, "hi").is_err(), "prompt {bad:?} refused");
            assert!(backend.abort(bad).is_err(), "abort {bad:?} refused");
        }

        // A deleted (Closed) session refuses prompts and aborts loudly.
        let dead = backend.create_session(&json!({})).unwrap();
        session
            .delete_session(SessionId::new(dead.parse().unwrap()))
            .unwrap();
        assert!(backend.prompt(&dead, "hi").is_err());
        assert!(backend.abort(&dead).is_err());

        // None of the hostile inputs touched the live session or crashed the
        // daemon: a real turn still completes.
        let result = backend.prompt(&sid, "still alive").unwrap();
        assert_eq!(result["finalState"], "ready_for_next_turn");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acp_session_prompt_uses_the_shadow_and_keeps_the_owner_untouched() {
        // Ordinary chat through ACP `session/prompt` goes through the SAME
        // PromptExecutionService as Native and SDK compat: its write lands
        // in the daemon-owned shadow of the session workspace; the owner
        // checkout stays byte-untouched until a verified integration.
        const OWNER: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 40;\n    let increment: u64 = 1;\n    base_amount.saturating_add(increment)\n}\n";
        const IMPL: &str = "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n";
        let rig = acp_shadow_rig(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: json!({"path": "src/lib.rs", "content": IMPL}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]);
        let owner = rig.dir.path().join("owner");
        std::fs::create_dir_all(owner.join("src")).unwrap();
        std::fs::write(owner.join("src/lib.rs"), OWNER).unwrap();
        let sid = rig
            .backend
            .create_session(&json!({"workspace": owner.to_str().unwrap()}))
            .unwrap();

        let result = rig.backend.prompt(&sid, "implement the change").unwrap();
        assert_eq!(result["status"], "completed", "{result}");
        let sid = SessionId::new(sid.parse().unwrap());
        let shadow = rig
            .session
            .shadow_row(sid)
            .unwrap()
            .expect("an ordinary mutating ACP prompt must begin a shadow");
        assert_eq!(shadow.state, faktor_session::ShadowRowState::Active);
        assert_eq!(
            std::fs::read(std::path::Path::new(&shadow.root).join("src/lib.rs")).unwrap(),
            IMPL.as_bytes(),
            "the edit landed in the shadow"
        );
        assert_eq!(
            std::fs::read(owner.join("src/lib.rs")).unwrap(),
            OWNER.as_bytes(),
            "the owner checkout is byte-untouched"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sdk_and_acp_prompts_hit_the_same_execution_service() {
        // Identity spy: ACP drives the exact PromptExecutionService instance
        // it was constructed with, and the service method the SDK compat
        // surface calls executes on the SAME task/session authorities.
        use faktor_server::native::{set_prompt_observer, PromptCallKind};
        use std::sync::Mutex;
        let rig = acp_shadow_rig(vec![
            ScriptedResponse::Text("pong".into()),
            ScriptedResponse::End,
        ]);
        let tasks_ptr = Arc::as_ptr(rig.service.tasks()) as usize;
        let sessions_ptr = Arc::as_ptr(rig.service.sessions()) as usize;
        let seen: Arc<Mutex<Vec<(PromptCallKind, usize, usize)>>> = Arc::new(Mutex::new(vec![]));
        let sink = seen.clone();
        set_prompt_observer(Some(Arc::new(move |call| {
            if call.tasks_ptr == tasks_ptr {
                sink.lock()
                    .unwrap()
                    .push((call.kind, call.tasks_ptr, call.sessions_ptr));
            }
        })));
        let owner = rig.dir.path().join("owner");
        std::fs::create_dir_all(&owner).unwrap();
        let sid = rig
            .backend
            .create_session(&json!({"workspace": owner.to_str().unwrap()}))
            .unwrap();
        // ACP session/prompt.
        rig.backend.prompt(&sid, "ping").unwrap();
        // Durable-point synchronization (load flake): the ACP prompt returns
        // when the turn MACHINE settles, but the drive's durable turn record
        // can still be ACTIVE for a moment (the detached drive resolves it on
        // its way out). A prompt issued in that window is correctly refused by
        // the executor's interrupted-run guard ("a live shadow ... and an
        // active drive"), so wait on the SAME durable point the guard reads
        // (the session's active turn record) before issuing the SDK call. The
        // retained shadow row is expected — a later prompt settles it. Bounded:
        // a run that never settles fails loudly here.
        {
            let sid = SessionId::new(sid.parse().unwrap());
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                let mid_turn = rig
                    .session
                    .get_session(sid)
                    .unwrap()
                    .expect("the session exists")
                    .active_turn_record()
                    .unwrap()
                    .is_some();
                if !mid_turn {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the first ACP run's drive never settled (active turn record \
                     still durable)"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        // The SDK compat surface's exact service call (the server builds
        // its facade over the same deps and calls `prompt`).
        let request = faktor_server::native::PromptRequest {
            prompt: "sdk ping".into(),
            ..Default::default()
        };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                rig.service
                    .prompt(SessionId::new(sid.parse().unwrap()), request),
            )
        })
        .unwrap();
        set_prompt_observer(None);
        let calls = seen.lock().unwrap().clone();
        assert!(
            calls.len() >= 2,
            "ACP and the SDK service call must both be observed: {calls:?}"
        );
        for (kind, tasks, sessions) in &calls {
            assert_eq!(*tasks, tasks_ptr, "call {kind:?} ran on another executor");
            assert_eq!(
                *sessions, sessions_ptr,
                "call {kind:?} ran on another store"
            );
        }
        // The backend's field is the same Arc the caller constructed.
        assert!(Arc::ptr_eq(&rig.backend.prompts, &rig.service));
    }

    #[test]
    fn acp_production_never_drives_the_agent_directly() {
        // Static scan (work-entry unification): the ACP host's backend may
        // only translate wire prompts into PromptExecutionService calls —
        // no direct AgentRuntime drive entry may remain in its body.
        let src = include_str!("main.rs");
        let lines: Vec<&str> = src.lines().collect();
        let start = lines
            .iter()
            .position(|l| l.contains("impl AcpBackend for DaemonAcpBackend {"))
            .expect("the ACP backend impl exists");
        // The trait impl runs until the first column-0 closing brace after
        // its header (it contains no nested column-0 items).
        let end = lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, l)| l.starts_with('}'))
            .map(|(j, _)| j)
            .expect("the ACP backend impl closes");
        for (i, l) in lines[start..end].iter().enumerate() {
            for token in [
                ".run_session_queue(",
                ".drive_receipt(",
                "agent.submit(",
                ".run_turn(",
            ] {
                assert!(
                    !l.contains(token),
                    "DaemonAcpBackend:{}: direct agent drive {token:?}: {l}",
                    start + i + 1
                );
            }
        }
        assert!(
            lines[start..end]
                .iter()
                .any(|l| l.contains("service.prompt(")),
            "The ACP prompt must be delegated to PromptExecutionService::prompt"
        );
    }

    /// Write one fake complete backup `faktor-plus-{ts_ms}.db` of `size`
    /// bytes with an explicit mtime (deterministic ordering for gate and
    /// retention tests).
    fn write_backup(
        dir: &std::path::Path,
        ts_ms: u64,
        mtime: std::time::SystemTime,
        size: u64,
    ) -> std::path::PathBuf {
        let p = dir.join("backups").join(format!("faktor-plus-{ts_ms}.db"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![0u8; size as usize]).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(mtime).unwrap();
        p
    }

    #[test]
    fn backup_gate_skips_a_fresh_matching_snapshot_and_honors_staleness_and_size() {
        let hour = std::time::Duration::from_secs(3600);
        // Fresh store for each scenario so mtime ordering stays unambiguous.
        // (a) No backups at all: due.
        {
            let dir = tempfile::tempdir().unwrap();
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            drop(session);
            assert!(backup_due(dir.path()), "no backups: startup backup is due");
        }
        // (b) A fresh backup whose size matches the store: NOT due (audit 44
        // interval gate — rapid restarts must not pile hourly snapshots).
        {
            let dir = tempfile::tempdir().unwrap();
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
                .unwrap()
                .len();
            drop(session);
            write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
            assert!(
                !backup_due(dir.path()),
                "fresh same-size backup must gate the snapshot"
            );
        }
        // (c) Same size but old: due.
        {
            let dir = tempfile::tempdir().unwrap();
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
                .unwrap()
                .len();
            drop(session);
            write_backup(
                dir.path(),
                1,
                std::time::SystemTime::now() - 2 * hour,
                db_len,
            );
            assert!(backup_due(dir.path()), "old snapshot is stale: due");
        }
        // (d) Fresh but the store RESIZED since the snapshot (crash-recovery
        // wrote): due even inside the interval.
        {
            let dir = tempfile::tempdir().unwrap();
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
                .unwrap()
                .len();
            drop(session);
            write_backup(
                dir.path(),
                1,
                std::time::SystemTime::now() - std::time::Duration::from_secs(600),
                db_len - 1,
            );
            assert!(
                backup_due(dir.path()),
                "a resized store makes a fresh snapshot stale: due"
            );
        }
    }

    #[test]
    fn rotate_backup_enforces_the_count_quota_and_keeps_the_newest() {
        // 10 old backups + the new snapshot = 11 candidates; retention must
        // drop the OLDEST until BACKUP_MAX_FILES (8) remain — never the
        // snapshot just written.
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let store = session.store();
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        let now = std::time::SystemTime::now();
        for i in 0..10u64 {
            write_backup(
                dir.path(),
                1000 + i,
                now - std::time::Duration::from_secs((i + 1) * 3600),
                100,
            );
        }
        rotate_backup(&store, dir.path());
        let files = list_backups(dir.path());
        assert_eq!(
            files.len(),
            BACKUP_MAX_FILES,
            "11 candidates must be rotated down to {BACKUP_MAX_FILES}"
        );
        // The newest survivor is the snapshot just written (matches the
        // store size; the seeded fakes are 100 bytes).
        let newest_len = std::fs::metadata(&files[0]).unwrap().len();
        assert_eq!(newest_len, db_len, "the fresh snapshot must survive");
        // No interrupted-writer temp files are ever listed or kept.
        assert!(files.iter().all(|p| !p.to_string_lossy().contains(".tmp-")));
    }

    #[test]
    fn backup_finalize_is_one_atomic_adoption_and_never_lists_partial_temp() {
        // A crashed writer's `.db.tmp-*` is invisible to the gate/retention
        // scans; the finalize step publishes it whole (fsync + rename +
        // directory fsync through the shared authority) and consumes it.
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let store = session.store();
        // Seed a fresh same-size fake so the gate would otherwise skip.
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
        let backups = dir.path().join("backups");
        // Crash residue: a partially written snapshot under the temp name.
        let tmp = backups.join(format!("faktor-plus-999.db.tmp-{}", std::process::id()));
        std::fs::write(&tmp, b"partial sqlite pages").unwrap();
        assert!(
            !list_backups(dir.path()).iter().any(|p| p == &tmp),
            "an in-progress temp is never a complete backup"
        );
        // The atomic adoption publishes it and consumes the temp.
        let dest = backups.join("faktor-plus-999.db");
        faktor_fs::atomic::atomic_adopt(&tmp, &dest).unwrap();
        assert!(!tmp.exists(), "the adopted temp is renamed, not copied");
        assert_eq!(std::fs::read(&dest).unwrap(), b"partial sqlite pages");
        assert!(list_backups(dir.path()).iter().any(|p| p == &dest));
        // A missing temp fails loudly and never mints a destination.
        let ghost = backups.join("faktor-plus-1000.db.tmp-ghost");
        assert!(
            faktor_fs::atomic::atomic_adopt(&ghost, &backups.join("faktor-plus-1000.db")).is_err()
        );
        assert!(!backups.join("faktor-plus-1000.db").exists());
        drop(store);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_line_precedes_backup_and_the_interval_gate_skips_a_fresh_snapshot() {
        // Audit 44: serve must print the startup line BEFORE any backup file
        // exists, and when the interval gate says skip, no backup may appear
        // even after the delayed backup task would have fired.
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let db_len = std::fs::metadata(dir.path().join("store").join("faktor-plus.db"))
            .unwrap()
            .len();
        drop(session);
        // Seed a fresh backup matching the store: the gate says skip.
        write_backup(dir.path(), 1, std::time::SystemTime::now(), db_len);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, None, Some(ready_tx), Some(shutdown_rx)).await
        });
        // The startup line is printed (readiness): no backup has run yet.
        tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        // Wait past the post-ready delay + margin: the gate must still skip.
        tokio::time::sleep(BACKUP_START_DELAY + std::time::Duration::from_millis(500)).await;
        assert_eq!(
            list_backups(dir.path()).len(),
            1,
            "the interval gate must skip the backup on a fresh snapshot"
        );
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_backup_runs_only_after_readiness_when_due() {
        // With no existing backup the startup backup IS due, but it must run
        // strictly AFTER the startup line: at readiness no backup file
        // exists yet; it appears only after the post-ready delay.
        let dir = tempfile::tempdir().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, dir2, None, Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        assert!(
            list_backups(dir.path()).is_empty(),
            "startup line must precede any backup file"
        );
        // The delayed task then writes exactly one snapshot.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if list_backups(dir.path()).len() == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the gated backup task must run after readiness"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(10), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }

    /// Serializes the real-signal tests: SIGTERM/SIGINT are delivered to the
    /// WHOLE process, so concurrent signal tests would cross-consume each
    /// other's signals (or worse, race an unarmed waiter).
    static SIGNAL_TEST_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Wait until `count` shutdown-signal waiters are armed (each successful
    /// SIGTERM/SIGINT registration increments the counter). Bounded.
    async fn wait_for_armed_signal_waiters(count: u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst) < count {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the shutdown-signal waiter was never armed (armed \
                 {})",
                SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst)
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn send_self_signal(signal: i32) {
        // SAFETY: `kill(2)` with our own pid and a valid signal number; the
        // signal tests install the tokio handlers BEFORE sending.
        unsafe {
            libc::kill(libc::getpid(), signal);
        }
    }

    /// Production serve has NO shutdown channel: SIGTERM must trigger the
    /// SAME full drain (`shutdown_serving_daemon`: worker-plane join, index
    /// worker join, drive drain, backup drain) and return cleanly — proving
    /// the production path is no longer `std::future::pending()`.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_sigterm_runs_the_full_drain_and_returns_clean() {
        let _guard = SIGNAL_TEST_GUARD.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let baseline = SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon =
            tokio::task::spawn(
                async move { serve_impl(0, dir2, None, Some(ready_tx), None).await },
            );
        tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        wait_for_armed_signal_waiters(baseline + 1).await;
        send_self_signal(libc::SIGTERM);
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("SIGTERM must trigger the bounded drain, never kill the process")
            .expect("the serve task must not panic");
        assert!(
            result.is_ok(),
            "the drained daemon returns cleanly: {result:?}"
        );
    }

    /// The second signal during the drain takes the FORCE path: it must
    /// report the force-exit code (the test probe replaces `process::exit`,
    /// which would kill the harness) while the first signal's bounded drain
    /// still completes cleanly.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn second_signal_during_the_drain_forces_exit_through_the_probe() {
        let _guard = SIGNAL_TEST_GUARD.lock().await;
        SIGNAL_DRAIN_STARTED.store(false, std::sync::atomic::Ordering::SeqCst);
        let (probe_tx, probe_rx) = tokio::sync::oneshot::channel();
        *FORCE_EXIT_PROBE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe_tx);
        let dir = tempfile::tempdir().unwrap();
        let baseline = SIGNAL_WAITERS_ARMED.load(std::sync::atomic::Ordering::SeqCst);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let dir2 = dir.path().to_path_buf();
        let daemon =
            tokio::task::spawn(
                async move { serve_impl(0, dir2, None, Some(ready_tx), None).await },
            );
        tokio::time::timeout(std::time::Duration::from_secs(30), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        wait_for_armed_signal_waiters(baseline + 1).await;
        send_self_signal(libc::SIGTERM);
        // Wait until the drain was entered (the watchdog is then spawned)
        // and its own signal waiter is armed before escalating.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        while !SIGNAL_DRAIN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the first signal never reached the drain"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        wait_for_armed_signal_waiters(baseline + 2).await;
        send_self_signal(libc::SIGTERM);
        let code = tokio::time::timeout(std::time::Duration::from_secs(10), probe_rx)
            .await
            .expect("the second signal must take the force path")
            .expect("the force probe must fire");
        assert_eq!(code, FORCE_EXIT_CODE);
        // The probe replaced the process exit: the first signal's drain
        // still completes cleanly in-process.
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("the drain must still finish")
            .expect("the serve task must not panic");
        assert!(
            result.is_ok(),
            "the drained daemon returns cleanly: {result:?}"
        );
    }

    /// The forced-exit path is reachable through the probe even without a
    /// serve task (classification seam).
    #[cfg(unix)]
    #[tokio::test]
    async fn force_signal_reports_the_force_exit_code() {
        let _guard = SIGNAL_TEST_GUARD.lock().await;
        let (probe_tx, probe_rx) = tokio::sync::oneshot::channel();
        *FORCE_EXIT_PROBE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(probe_tx);
        on_force_signal("SIGTERM").await;
        assert_eq!(probe_rx.await.unwrap(), FORCE_EXIT_CODE);
    }

    /// Serve startup queue recovery: a durable pending queue head left by a
    /// killed process (active turn already aborted, no live runner anywhere)
    /// is claimed by EXACTLY ONE runner as part of the serve startup
    /// sequence, with no new submit. The startup-wiring harness configures no
    /// provider (scripted providers are not a config surface), so the claimed
    /// head's turn fails closed on the missing model — exactly once; real
    /// end-to-end execution of a recovered head is proven one layer down
    /// (`task_executor_tests::relaunch_recovery_drains_a_pending_head_without_a_new_submit`,
    /// which asserts the provider call count is exactly one).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_startup_drains_a_pending_durable_queue_head_without_a_submit() {
        use faktor_core::event::EventKind;
        let dir = tempfile::tempdir().unwrap();
        // The kill residue: A is the active turn and B queues durably behind
        // it. Neither is ever driven; A is aborted so the machine is idle and
        // the pending head B is exactly what startup recovery must find.
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let ws = session.create_workspace("/w").unwrap();
        let sid = session
            .create_session(ws, "queue recovery", "fake", "m")
            .unwrap()
            .id();
        let agent = test_agent(session.clone(), ProviderRegistry::new());
        let receipt_a = agent.submit(sid, "A prompt", &[]).unwrap();
        let receipt_b = agent.submit(sid, "B prompt", &[]).unwrap();
        assert!(receipt_b.queued, "B queues behind A");
        agent.abort_op(sid, Some(receipt_a.op_id)).unwrap();
        let handle = session.get_session(sid).unwrap().unwrap();
        assert_eq!(
            handle.queued_prompt_count().unwrap(),
            1,
            "B survives the kill durably"
        );

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let data_dir = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, data_dir, None, Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");

        // Startup recovery hands the head to exactly one runner: the durable
        // row leaves the non-terminal set, with no submit after the seed.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        while handle.queued_prompt_count().unwrap() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "startup recovery must drain the pending head"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        // Grace window for any duplicate runner to show itself.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(
            handle.queued_prompt_count().unwrap(),
            0,
            "the head stays drained"
        );
        let counts = handle.queue_status_counts().unwrap();
        for status in ["pending", "claimed", "running"] {
            assert_eq!(counts.get(status), None, "queue row is terminal: {counts}");
        }
        let events = handle.events_range(1, None).unwrap();
        let count = |kind: EventKind| {
            events
                .iter()
                .filter(|e| e.kind == kind && e.op_id == Some(receipt_b.op_id))
                .count()
        };
        assert_eq!(
            count(EventKind::PromptReceived),
            1,
            "B was queued exactly once"
        );
        assert_eq!(
            count(EventKind::PromptAdmitted),
            1,
            "B was claimed exactly once"
        );
        let terminal = events
            .iter()
            .filter(|e| {
                e.op_id == Some(receipt_b.op_id)
                    && matches!(e.kind, EventKind::TurnCompleted | EventKind::Failed)
            })
            .count();
        assert_eq!(terminal, 1, "B's one admitted turn ended exactly once");

        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }

    /// Kill-relaunch integration: a FIRST full daemon (real graph, real
    /// executor) leaves B durably queued behind an aborted A, then its drive
    /// registry is shut down exactly like a process death. The relaunched
    /// `serve` startup path must claim B exactly once (one admitted turn),
    /// with no new submit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_startup_recovery_drains_a_killed_daemons_queue_head_on_relaunch() {
        use faktor_core::event::EventKind;
        let dir = tempfile::tempdir().unwrap();
        // First daemon process: real graph, A active, B durably queued behind
        // it, neither driven.
        let graph = build_daemon_with_mcp_and_chunks_fast(
            dir.path(),
            None,
            None,
            graph::SemanticCfg::default(),
        )
        .await
        .unwrap();
        let session = graph.session.clone();
        let ws = session.create_workspace("/w").unwrap();
        let sid = session
            .create_session(ws, "relaunch recovery", "fake", "m")
            .unwrap()
            .id();
        let receipt_a = graph.agent.submit(sid, "A prompt", &[]).unwrap();
        let receipt_b = graph.agent.submit(sid, "B prompt", &[]).unwrap();
        assert!(receipt_b.queued, "B queues behind A");
        // Kill: the drive registry is closed (every later settle kick is
        // refused), then the active turn is aborted so A is not resumed and
        // B's durable row is the only runnable marker left.
        let report = graph
            .tasks
            .shutdown_drives(std::time::Duration::from_millis(50))
            .await;
        assert!(
            report.all_reaped(),
            "the killed daemon reaped its drives: {report:?}"
        );
        graph.agent.abort_op(sid, Some(receipt_a.op_id)).unwrap();
        assert_eq!(
            session
                .get_session(sid)
                .unwrap()
                .unwrap()
                .queued_prompt_count()
                .unwrap(),
            1,
            "B survives the kill durably"
        );
        drop(graph);

        // Relaunch through the REAL serve startup path; no new submit.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let data_dir = dir.path().to_path_buf();
        let daemon = tokio::task::spawn(async move {
            serve_impl(0, data_dir, None, Some(ready_tx), Some(shutdown_rx)).await
        });
        tokio::time::timeout(std::time::Duration::from_secs(60), ready_rx)
            .await
            .expect("serve must reach the startup line")
            .expect("ready signal");
        // Observe the relaunched directory through a fresh manager (the
        // killed one is gone): the head drains with exactly one claim.
        let observer =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let handle = observer.get_session(sid).unwrap().unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        while handle.queued_prompt_count().unwrap() != 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "relaunch recovery must drain the killed head"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert_eq!(handle.queued_prompt_count().unwrap(), 0);
        let events = handle.events_range(1, None).unwrap();
        let admitted = events
            .iter()
            .filter(|e| e.kind == EventKind::PromptAdmitted && e.op_id == Some(receipt_b.op_id))
            .count();
        assert_eq!(admitted, 1, "the relaunched daemon claimed B exactly once");
        let terminal = events
            .iter()
            .filter(|e| {
                e.op_id == Some(receipt_b.op_id)
                    && matches!(e.kind, EventKind::TurnCompleted | EventKind::Failed)
            })
            .count();
        assert_eq!(terminal, 1, "B's one admitted turn ended exactly once");
        let _ = shutdown_tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(30), daemon)
            .await
            .expect("daemon must stop on shutdown")
            .expect("serve_impl returns Ok")
            .unwrap();
    }

    /// The residual: the REAL daemon shutdown sequence — the exact function
    /// serve invokes when its shutdown channel fires — must reap live
    /// orchestrated drives within the documented bound, leave the drive
    /// registry empty, and keep the aborted run recoverable from its durable
    /// rows (the task_executor abort->resume path, driven from the serve
    /// integration level over the production shadow-carrying executor).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn serve_shutdown_reaps_live_orchestrated_drives_and_keeps_them_resumable() {
        use faktor_core::id::{TaskId as CoreTaskId, WorktreeId};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Bounded wait for the drive to reach the provider call. Under
        /// full-suite CPU starvation the OS schedules the drive's progress
        /// arbitrarily late, so this is a CONDITION wait with an explicit
        /// generous deadline — never a fixed sleep, never a bound sized for
        /// an idle machine.
        const DRIVE_PARK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
        /// Wall-clock load allowance for observing the drain: the product
        /// bound (`SERVE_DRIVE_SHUTDOWN_GRACE + ABORT_REAP_GRACE`) is
        /// enforced by the registry's own deadlines, and scheduler
        /// starvation stretches wall-clock timers. The starvation-
        /// independent teeth stay strict: the parked drive is aborted (never
        /// completed), every drive is reaped, the registry is closed, and
        /// the durable rows resume.
        const SHUTDOWN_DRAIN_LOAD_ALLOWANCE: std::time::Duration =
            std::time::Duration::from_secs(60);

        /// Provider whose model calls PARK until the gate opens: the
        /// orchestrated child drives stay deterministically in flight across
        /// the shutdown window, so the drain MUST abort (never just await)
        /// them.
        struct GateProvider {
            caps: ModelCapabilities,
            opened: tokio::sync::watch::Sender<bool>,
            entered: Arc<AtomicUsize>,
            /// Wakes a condition waiter whenever `entered` advances; a
            /// `notify_one` permit is stored, so a call that entered before
            /// the wait began can never be missed.
            entered_notify: tokio::sync::Notify,
        }

        impl GateProvider {
            /// Wait, bounded by the explicit `deadline`, until at least
            /// `at_least` provider calls have entered. Returns `false` only
            /// when the deadline elapsed.
            async fn wait_entered(&self, at_least: usize, deadline: std::time::Duration) -> bool {
                let wait = async {
                    while self.entered.load(Ordering::SeqCst) < at_least {
                        self.entered_notify.notified().await;
                    }
                };
                tokio::time::timeout(deadline, wait).await.is_ok()
            }
        }

        impl Provider for GateProvider {
            fn id(&self) -> &str {
                "gate"
            }

            fn capabilities(&self, _model: &str) -> ModelCapabilities {
                self.caps.clone()
            }

            fn stream(&self, _req: GenericAgentRequest) -> faktor_provider::ProviderStream {
                self.entered.fetch_add(1, Ordering::SeqCst);
                self.entered_notify.notify_one();
                let mut rx = self.opened.subscribe();
                let s = futures::stream::once(async move {
                    while !*rx.borrow_and_update() {
                        if rx.changed().await.is_err() {
                            break;
                        }
                    }
                    Ok(ProviderChunk::Text {
                        text: "done".into(),
                    })
                })
                .chain(futures::stream::once(async { Ok(ProviderChunk::Done) }));
                Box::pin(s)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (opened, _opened_rx) = tokio::sync::watch::channel(false);
        let provider = Arc::new(GateProvider {
            caps: ModelCapabilities {
                tools: true,
                parallel_tools: true,
                ..Default::default()
            },
            opened,
            entered: Arc::new(AtomicUsize::new(0)),
            entered_notify: tokio::sync::Notify::new(),
        });

        // A real, shadow-carrying executor over a real store: the SAME
        // authority set the serve daemon graph builds.
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let mut registry = ProviderRegistry::new();
        registry.try_register(provider.clone()).unwrap();
        let agent = test_agent(session.clone(), registry);
        let owner_root = dir.path().join("owner");
        std::fs::create_dir_all(&owner_root).unwrap();
        std::fs::write(owner_root.join("a.txt"), b"base").unwrap();
        let ws = session
            .create_workspace(owner_root.to_str().unwrap())
            .unwrap();
        let wt = WorktreeId::new(
            session
                .put_worktree(ws, owner_root.to_str().unwrap(), "main")
                .unwrap() as u64,
        );
        let parent = session
            .create_session(ws, "serve-shutdown", "gate", "m")
            .unwrap()
            .id();
        session
            .adopt_identity(parent, wt, CoreTaskId::new(1))
            .unwrap();
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
        let shadows = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
            session.clone(),
            dir.path().join("shadows"),
        )
        .unwrap();
        let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent.clone(),
            shadows,
        );

        // Two work items => a real orchestrated run whose child drives park
        // inside the provider call.
        let receipt = tasks
            .start_task(
                parent,
                faktor_orchestrator::runtime::task_executor::TaskRunRequest {
                    goal: "two-item shutdown run".into(),
                    work_items: vec![
                        faktor_orchestrator::WorkItem::new(
                            "a",
                            "work a",
                            faktor_orchestrator::WorkKind::Analysis,
                        ),
                        faktor_orchestrator::WorkItem::new(
                            "b",
                            "work b",
                            faktor_orchestrator::WorkKind::Analysis,
                        ),
                    ],
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            receipt.mode,
            faktor_orchestrator::runtime::task_executor::TaskRunMode::Orchestrated
        );
        let run_id = receipt.run_id.clone();
        // The drive is live AND parked in the provider: it cannot finish on
        // its own before shutdown. Wait on the observable park condition
        // (the provider call entered) with the explicit deadline above —
        // under full-suite CPU starvation this can take far longer than on
        // an idle machine, and a fixed 30s window was the last known flake.
        assert!(
            provider.wait_entered(1, DRIVE_PARK_DEADLINE).await,
            "the orchestrated drive never parked within {DRIVE_PARK_DEADLINE:?}"
        );
        assert_eq!(
            tasks.live_drive_count(),
            1,
            "the parked drive is owned by the executor registry"
        );
        // The durable assignments (the resume entry point) exist BEFORE the
        // abort.
        let assignments = faktor_orchestrator::runtime::OrchestratorRuntime::assignment_rows(
            session.clone(),
            parent,
            &run_id,
        )
        .expect("assignment rows readable");
        assert!(
            !assignments.is_empty(),
            "durable assignments before shutdown"
        );

        // The EXACT serve shutdown sequence (the post-ready loop handles are
        // dummies; the daemon's own loops have dedicated tests). The drain
        // itself must resolve within the documented product bound plus the
        // explicit load allowance; a timeout here is a real unbounded-
        // shutdown failure, never an unbounded test wait.
        let drain_bound = SERVE_DRIVE_SHUTDOWN_GRACE
            + faktor_orchestrator::runtime::task_executor::TaskDriveRegistry::ABORT_REAP_GRACE
            + SHUTDOWN_DRAIN_LOAD_ALLOWANCE;
        let report = tokio::time::timeout(
            drain_bound,
            shutdown_serving_daemon(
                &tasks,
                None,
                None,
                None,
                None,
                tokio::spawn(std::future::pending::<()>()),
                None,
                None,
                tokio::spawn(async {}),
            ),
        )
        .await
        .expect("the drive drain must stay within grace + abort/reap + load allowance");
        assert!(report.total >= 1, "the live drive was owned: {report:?}");
        assert_eq!(
            report.completed, 0,
            "the parked drive must not complete during the grace: {report:?}"
        );
        assert!(
            report.aborted >= 1,
            "the parked drive was aborted: {report:?}"
        );
        assert_eq!(report.unreaped, 0, "every drive reaped: {report:?}");
        assert_eq!(tasks.live_drive_count(), 0, "registry empty");
        assert!(tasks.live_drive_runs().is_empty(), "registry empty");
        assert!(tasks.drives_shutdown(), "the registry is closed");

        // Durable state intact: the assignment rows survive the abort.
        let after = faktor_orchestrator::runtime::OrchestratorRuntime::assignment_rows(
            session.clone(),
            parent,
            &run_id,
        )
        .expect("assignment rows still readable");
        assert!(!after.is_empty(), "durable assignments survive the abort");

        // CRASH: drop the whole daemon stack and reopen the same data dir —
        // exactly a daemon restart. The aborted run re-attaches from its
        // durable rows through the documented recovery entry.
        drop(tasks);
        drop(orchestrator);
        drop(agent);
        drop(session);
        let session2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let mut registry2 = ProviderRegistry::new();
        registry2.try_register(provider.clone()).unwrap();
        let agent2 = test_agent(session2.clone(), registry2);
        agent2.recover().expect("fresh daemon recovery pass");
        let orchestrator2 = faktor_orchestrator::runtime::OrchestratorRuntime::new(
            session2.clone(),
            agent2.clone(),
        );
        let shadows2 = faktor_orchestrator::runtime::shadow::ShadowRoots::new(
            session2.clone(),
            dir.path().join("shadows"),
        )
        .unwrap();
        let tasks2 = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator2,
            session2.clone(),
            agent2.clone(),
            shadows2,
        );
        let resumed = tasks2
            .resume_run(
                parent,
                &run_id,
                faktor_orchestrator::runtime::Ceilings::default(),
                faktor_orchestrator::caps::CapabilitySet::new(),
                None,
            )
            .expect("the aborted run re-attaches from its durable rows");
        assert_eq!(resumed.run_id, run_id);
        // Re-attachment is only proven when the re-driven child observably
        // reaches the provider again: the resumed drive re-parks on the same
        // closed gate, so it stays owned by the fresh registry.
        assert!(
            provider.wait_entered(2, DRIVE_PARK_DEADLINE).await,
            "the re-attached run never re-parked within {DRIVE_PARK_DEADLINE:?}"
        );
        assert_eq!(
            tasks2.live_drive_count(),
            1,
            "the re-attached drive is owned by the fresh registry"
        );
        // Reap the re-attached drive too: the second drain owns, aborts and
        // reaps it exactly like the first.
        let report2 = tasks2
            .shutdown_drives(std::time::Duration::from_millis(50))
            .await;
        assert!(
            report2.total >= 1,
            "the re-attached drive was owned: {report2:?}"
        );
        assert_eq!(
            report2.completed, 0,
            "the re-parked drive must not complete during the grace: {report2:?}"
        );
        assert!(
            report2.aborted >= 1,
            "the re-parked drive was aborted: {report2:?}"
        );
        assert_eq!(
            report2.unreaped, 0,
            "every re-attached drive reaped: {report2:?}"
        );
        assert_eq!(tasks2.live_drive_count(), 0);
    }

    #[test]
    fn doctor_plain_passes_on_a_healthy_store_and_fails_on_corruption() {
        let dir = tempfile::tempdir().unwrap();
        {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            session
                .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
                .unwrap();
        }
        let report = doctor_run(dir.path(), false);
        assert_eq!(report.issues, 0, "healthy store: {:?}", report.lines);
        assert!(report.lines.iter().any(|l| l == "store: ok"));
        assert!(report
            .lines
            .iter()
            .any(|l| l.contains("\"journal_mode\": \"wal\"")));
        // Corrupt store: plain doctor fails loudly (never exits the process
        // from doctor_run; the wrapper owns the exit code).
        let garbage = tempfile::tempdir().unwrap();
        std::fs::write(
            garbage.path().join("store"),
            b"not a directory; the open must fail cleanly",
        )
        .unwrap();
        let report = doctor_run(garbage.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(report.lines.iter().any(|l| l.starts_with("store: FAILED")));
    }

    /// The additive `doctor --config` worker-plane boundary audit: the
    /// resolved exposure decision (with the trusted-gateway acknowledgement)
    /// is reported, and a refused boundary is an ISSUE — never repaired.
    #[test]
    fn doctor_reports_the_worker_plane_boundary_and_acknowledgement() {
        let dir = tempfile::tempdir().unwrap();
        {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
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
            report.lines.iter().any(|line| line
                .contains("worker plane: trusted_gateway acknowledgement recorded")),
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

    /// F9 (adversarial): a config the DAEMON refuses (semantic validation
    /// failure) must never be reported as an enabled worker plane — the
    /// doctor runs serve's own load+validate path and reports the refused
    /// state with an issue.
    #[test]
    fn doctor_refuses_a_config_the_daemon_would_refuse() {
        let dir = tempfile::tempdir().unwrap();
        {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            session
                .create_session(session.create_workspace("/w").unwrap(), "t", "p", "m")
                .unwrap();
        }
        let config = dir.path().join("config.json");
        // Duplicate provider ids PARSE but `Config::validate` refuses them:
        // serve exits with a config error, so the doctor must refuse too.
        std::fs::write(
            &config,
            r#"{"model": "m", "providers": [{"kind": "ollama", "id": "dup"}, {"kind": "ollama", "id": "dup"}], "cloud": {"enabled": true}, "workers": {"enabled": true, "organization": "org_local"}, "worker_plane": {"enabled": true}}"#,
        )
        .unwrap();
        let report = doctor_run_with_config(dir.path(), false, Some(&config));
        assert!(
            report
                .lines
                .iter()
                .any(|line| line.starts_with("worker plane: FAILED")
                    && line.contains("state=refused")
                    && line.contains("config_refused")),
            "the refused config names the refused state: {:?}",
            report.lines
        );
        assert!(
            !report
                .lines
                .iter()
                .any(|line| line.starts_with("worker plane: enabled")),
            "a config the daemon refuses must never report the plane as enabled: {:?}",
            report.lines
        );
        assert!(report.issues >= 1, "{:?}", report.lines);
    }
    /// The additive `cloud-db` doctor section (P1 durability): a real
    /// control-plane database is reported with its writer-recorded
    /// `synchronous=FULL` policy, integrity, verified backup age and its
    /// SELF-CONSISTENT migration restore point (fresh creation writes a
    /// verified `-pre-migration-v0-` point); the LOCAL-authority caveat is
    /// printed.
    #[test]
    fn doctor_reports_the_commercial_db_section_with_policy_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
        let report = doctor_run(dir.path(), false);
        assert_eq!(report.issues, 0, "healthy: {:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("LOCAL commercial deployment authority")
                    && l.starts_with("cloud-db:")),
            "{:?}",
            report.lines
        );
        let section = report
            .lines
            .iter()
            .find(|l| l.starts_with("cloud-db control-plane.db:"))
            .expect("the section names the database");
        assert!(section.contains("synchronous=FULL"), "{section}");
        assert!(section.contains("journal_mode=wal"), "{section}");
        assert!(section.contains("integrity=ok"), "{section}");
        assert!(report
            .lines
            .iter()
            .any(|l| l.contains("last verified backup") && l.contains("control-plane")));
        assert!(report
            .lines
            .iter()
            .any(|l| l.contains("migration restore point") && l.contains("-pre-migration-v0-")));
    }

    /// Doctor fails loudly (naming the DB) when the newest verified rotating
    /// backup is older than the threshold: a backup that old no longer bounds
    /// the loss window.
    #[test]
    fn doctor_fails_loudly_on_a_stale_rotating_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
        let (backup, _) = faktor_cloud::durability::latest_backup(&path)
            .expect("the first open writes a verified rotating backup");
        let aged = std::time::SystemTime::now()
            - std::time::Duration::from_secs(faktor_cloud::durability::BACKUP_MAX_AGE_SECS + 60);
        std::fs::File::options()
            .write(true)
            .open(&backup)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        let report = doctor_run(dir.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db control-plane.db:") && l.contains("STALE")),
            "{:?}",
            report.lines
        );
    }

    /// Doctor RE-VERIFIES the restore point and fails loudly (naming the DB)
    /// when it is missing while the recorded policy requires one, or when its
    /// content does not match its name's version claim.
    #[test]
    fn doctor_fails_loudly_on_missing_or_mislabeled_restore_points() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        drop(faktor_cloud::SqliteControlPlaneStore::open(&path).unwrap());
        let (point, _) =
            faktor_cloud::durability::latest_migration_backup(&path).expect("v0 restore point");
        assert!(point
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("-pre-migration-v0-"));

        // Missing while required: doctor fails and names the database.
        std::fs::remove_file(&point).unwrap();
        let report = doctor_run(dir.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db control-plane.db:")
                    && l.contains("migration restore point FAILED verification")
                    && l.contains("no pre-migration restore point exists")),
            "{:?}",
            report.lines
        );

        // Mislabeled: a full copy of the LIVE (post-migration) database under
        // a `-pre-migration-v4-` name must fail verification, naming the DB.
        let backup_dir = faktor_cloud::durability::backup_dir(&path);
        std::fs::create_dir_all(&backup_dir).unwrap();
        let mislabeled = backup_dir.join(format!(
            "control-plane-pre-migration-v4-{}-9.db",
            std::process::id()
        ));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            faktor_cloud::durability::backup_to(&conn, &mislabeled).unwrap();
        }
        let report = doctor_run(dir.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db control-plane.db:")
                    && l.contains("migration restore point FAILED verification")
                    && l.contains("mislabeled")),
            "{:?}",
            report.lines
        );
    }

    /// A corrupt commercial database fails doctor loudly and the `cloud-db`
    /// section names it (never silently skipped, never auto-repaired).
    #[test]
    fn doctor_fails_loudly_on_a_corrupt_commercial_db_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("control-plane.db"),
            b"garbage that is not a sqlite database",
        )
        .unwrap();
        let report = doctor_run(dir.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db control-plane.db: FAILED")),
            "{:?}",
            report.lines
        );
    }

    /// A legacy commercial database with no policy marker is flagged (its
    /// last writer did not acknowledge with `synchronous = FULL`).
    #[test]
    fn doctor_flags_a_cloud_family_db_without_the_policy_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE legacy (id TEXT PRIMARY KEY); PRAGMA journal_mode = WAL;",
            )
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

    /// The nested discovery probe: databases below the data root (workspace /
    /// session / cloud paths) are found with a bounded depth and a stable,
    /// deterministic order; hidden/backup trees, in-progress temp files and
    /// paths beyond the depth bound are never probed.
    #[test]
    fn doctor_discovers_nested_databases_deterministically_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        // A nested cloud-family database (marker + verified backup) ...
        let cloud_dir = dir.path().join("cloud-state");
        std::fs::create_dir_all(&cloud_dir).unwrap();
        drop(
            faktor_cloud::SqliteControlPlaneStore::open(&cloud_dir.join("control-plane.db"))
                .unwrap(),
        );
        // ... and a nested SCM store database (its own child backup tree).
        let scm_dir = dir.path().join("store").join("nested");
        std::fs::create_dir_all(&scm_dir).unwrap();
        drop(faktor_scm::SqliteScmStore::open(&scm_dir.join("scm.db")).unwrap());
        // Below the depth bound: `a/b/c/d/buried.db` (depth 4) is not probed.
        let deep = dir.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        {
            let conn = rusqlite::Connection::open(deep.join("buried.db")).unwrap();
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
                .unwrap();
        }
        // In-progress snapshots are invisible: the name carries `.tmp`.
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        {
            let conn = rusqlite::Connection::open(scratch.join("half.db.tmp")).unwrap();
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
                .unwrap();
        }
        let rels = |report: &DoctorReport| -> Vec<String> {
            report
                .lines
                .iter()
                .filter_map(|l| {
                    let rest = l.strip_prefix("cloud-db ")?;
                    let (rel, _) = rest.split_once(": journal_mode=")?;
                    Some(rel.to_string())
                })
                .collect()
        };
        let first = doctor_run(dir.path(), false);
        let second = doctor_run(dir.path(), false);
        let first_rels = rels(&first);
        assert_eq!(
            first_rels,
            rels(&second),
            "nested discovery order must be deterministic"
        );
        assert!(
            first_rels.contains(&"cloud-state/control-plane.db".to_string()),
            "{:?}",
            first.lines
        );
        assert!(
            first_rels.contains(&"store/nested/scm.db".to_string()),
            "{:?}",
            first.lines
        );
        assert!(
            !first_rels.iter().any(|rel| rel.contains("buried")),
            "beyond the depth bound: {:?}",
            first.lines
        );
        assert!(
            !first_rels.iter().any(|rel| rel.contains("half")),
            "temp snapshots are not probed: {:?}",
            first.lines
        );
        let nested = first
            .lines
            .iter()
            .find(|l| l.starts_with("cloud-db store/nested/scm.db:"))
            .expect("the nested store database is named with its relative path");
        assert!(nested.contains("journal_mode=wal"), "{nested}");
        assert!(nested.contains("synchronous=FULL"), "{nested}");
        assert!(nested.contains("integrity=ok"), "{nested}");
        assert!(
            first
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db store/nested/scm.db: last verified backup")),
            "{:?}",
            first.lines
        );
        assert_eq!(
            first.issues, 0,
            "healthy nested databases: {:?}",
            first.lines
        );
    }

    /// A corrupt nested database fails doctor loudly and is named by its
    /// data-root-relative path (never silently skipped, never auto-repaired).
    #[test]
    fn doctor_fails_loudly_on_a_corrupt_nested_db_naming_it() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("store").join("sessions");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("broken.db"),
            b"garbage that is not a sqlite database",
        )
        .unwrap();
        let report = doctor_run(dir.path(), false);
        assert!(report.issues >= 1, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db store/sessions/broken.db: FAILED")),
            "{:?}",
            report.lines
        );
    }

    /// Doctor reports the SCM / worker / updater stores with their
    /// writer-recorded policy, integrity, a verified backup age and their
    /// pre-migration restore point — the three DBs that previously appeared
    /// as unmarked.
    #[test]
    fn doctor_reports_policy_backup_and_restore_points_for_the_three_stores() {
        let dir = tempfile::tempdir().unwrap();
        let scm_path = dir.path().join("scm.db");
        drop(faktor_scm::SqliteScmStore::open(&scm_path).unwrap());
        let worker_path = dir.path().join("workers.db");
        drop(faktor_worker::SqliteWorkerStore::open(&worker_path).unwrap());
        let updater_path = dir.path().join("update.db");
        drop(faktor_updater::SqliteUpdaterStore::open(&updater_path).unwrap());
        // Roll each cursor back one version and reopen: the reopen writes a
        // VERIFIED pre-migration restore point before migrating, exactly the
        // state doctor must surface.
        for (path, from_version) in [
            (&scm_path, 1i64),
            (&worker_path, 1i64),
            (&updater_path, 2i64),
        ] {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {from_version}"))
                .unwrap();
            drop(conn);
        }
        drop(faktor_scm::SqliteScmStore::open(&scm_path).unwrap());
        drop(faktor_worker::SqliteWorkerStore::open(&worker_path).unwrap());
        drop(faktor_updater::SqliteUpdaterStore::open(&updater_path).unwrap());
        let report = doctor_run(dir.path(), false);
        for rel in ["scm.db", "workers.db", "update.db"] {
            let section = report
                .lines
                .iter()
                .find(|l| l.starts_with(&format!("cloud-db {rel}:")))
                .unwrap_or_else(|| panic!("{rel} is reported: {:?}", report.lines));
            assert!(section.contains("journal_mode=wal"), "{section}");
            assert!(section.contains("synchronous=FULL"), "{section}");
            assert!(section.contains("integrity=ok"), "{section}");
            assert!(
                report
                    .lines
                    .iter()
                    .any(|l| l.starts_with(&format!("cloud-db {rel}: last verified backup"))),
                "{rel} backup age: {:?}",
                report.lines
            );
            assert!(
                report.lines.iter().any(|l| l
                    .starts_with(&format!("cloud-db {rel}: migration restore point"))
                    && l.contains("-pre-migration-v")),
                "{rel} restore point: {:?}",
                report.lines
            );
        }
        assert_eq!(report.issues, 0, "{:?}", report.lines);
    }

    /// The probe budget is a bound, not a silent truncation: databases past
    /// it are counted, named as unprobed, and the run fails loudly.
    #[test]
    fn doctor_bounds_the_database_probe_budget_and_names_the_excess() {
        let dir = tempfile::tempdir().unwrap();
        let many = dir.path().join("many");
        std::fs::create_dir_all(&many).unwrap();
        for i in 0..33 {
            let conn = rusqlite::Connection::open(many.join(format!("db-{i:02}.db"))).unwrap();
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
                .unwrap();
        }
        let report = doctor_run(dir.path(), false);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("beyond the 32-database probe budget were NOT probed")),
            "{:?}",
            report.lines
        );
        assert!(report.issues >= 1, "{:?}", report.lines);
        // The probed subset itself stays named and healthy.
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.starts_with("cloud-db many/db-00.db:")),
            "{:?}",
            report.lines
        );
    }

    /// The `index embeddings` doctor section renders the TYPED status of the
    /// published generation exactly as `IndexView::embedding_index(ws)
    /// .build_status()` exposes it: a provider-failure fixture renders
    /// `degraded` with the affected chunk count and the bounded reason, a
    /// working source renders `complete`, and a build with no configured
    /// source renders `unconfigured` — never a silent lexical-only success.
    #[test]
    fn doctor_reports_the_published_index_embedding_build_status_typed_states() {
        use faktor_core::error::{Error, ErrorKind};
        use faktor_index::embedding::{EmbeddingBuildStatus, EmbeddingModel, EmbeddingSource};
        use faktor_index::IndexService;

        struct FailingSource;
        impl EmbeddingSource for FailingSource {
            fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                Err(Error::new(
                    ErrorKind::Provider {
                        code: "embedding_backend_down".into(),
                        retryable: false,
                    },
                    "embedding backend down",
                ))
            }
        }
        struct WorkingSource;
        impl EmbeddingSource for WorkingSource {
            fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, Error> {
                Ok(texts.iter().map(|_| vec![0.25, 0.5, 0.75, 1.0]).collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        // The daemon derives its index data root from the store path; the
        // doctor reads exactly that root.
        let index = IndexService::open(
            session.store(),
            dir.path().join("store").join("index_data"),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();

        let build = |name: &str, source: Option<Arc<dyn EmbeddingSource>>| -> u64 {
            let root = dir.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("lib.rs"), b"pub fn fixture() -> i64 { 1 }\n").unwrap();
            let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
            index.set_embedding_source(source, EmbeddingModel::new("fixture-model", "r1"));
            let view = index
                .ensure_ready(
                    ws,
                    std::time::Instant::now() + std::time::Duration::from_secs(120),
                )
                .unwrap();
            // The fixture asserts the SAME typed chain the doctor renders off
            // the published generation. A source-less, prior-less build
            // installs NO embedding map entry in the live view (callers read
            // that absence as "no embeddings" = unconfigured), while the
            // persisted generation carries the typed `Unconfigured` record
            // the doctor reads; both spell the same state.
            let status = view
                .index()
                .lock()
                .unwrap()
                .embedding_index(ws)
                .map(|index| index.build_status())
                .unwrap_or(EmbeddingBuildStatus::Unconfigured);
            match name {
                "degraded_repo" => assert!(
                    matches!(status, EmbeddingBuildStatus::Degraded { affected: 1, .. }),
                    "the failing fixture must persist a degraded status: {status:?}"
                ),
                "complete_repo" => assert!(
                    matches!(status, EmbeddingBuildStatus::Complete { embedded: 1, .. }),
                    "the working fixture must persist a complete status: {status:?}"
                ),
                _ => assert_eq!(status, EmbeddingBuildStatus::Unconfigured),
            }
            ws.raw()
        };
        let degraded = build("degraded_repo", Some(Arc::new(FailingSource)));
        let complete = build("complete_repo", Some(Arc::new(WorkingSource)));
        let unconfigured = build("unconfigured_repo", None);
        drop(index);
        drop(session);

        let report = doctor_run(dir.path(), false);
        let line_for = |raw: u64| {
            report
                .lines
                .iter()
                .find(|l| l.contains(&format!("workspace {raw}:")))
                .unwrap_or_else(|| panic!("no index line for workspace {raw}: {:?}", report.lines))
                .clone()
        };
        let degraded_line = line_for(degraded);
        assert!(
            degraded_line.contains(": degraded affected=1"),
            "{degraded_line}"
        );
        assert!(
            degraded_line.contains("reason=")
                && degraded_line.contains("embedding_backend_down")
                && degraded_line.contains("embedding backend down"),
            "{degraded_line}"
        );
        let complete_line = line_for(complete);
        assert!(complete_line.contains(": complete"), "{complete_line}");
        assert!(complete_line.contains("embedded=1"), "{complete_line}");
        let unconfigured_line = line_for(unconfigured);
        assert!(
            unconfigured_line.contains(": unconfigured"),
            "{unconfigured_line}"
        );
        assert_eq!(report.issues, 0, "{:?}", report.lines);
    }

    /// Doctor never trusts the index data it reads: a corrupt published
    /// generation is named by workspace and counted as an issue, a numeric
    /// generation directory with no durable state row is flagged, and
    /// non-workspace junk (non-numeric names, the reserved id 0) is ignored
    /// without a line or a panic.
    #[test]
    fn doctor_surfaces_corrupt_and_unmanaged_index_generations_without_panicking() {
        use faktor_index::IndexService;

        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas")).unwrap();
        let index_root = dir.path().join("store").join("index_data");
        let index = IndexService::open(
            session.store(),
            index_root.clone(),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), b"pub fn corrupt_me() -> i64 { 1 }\n").unwrap();
        let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
        index
            .ensure_ready(
                ws,
                std::time::Instant::now() + std::time::Duration::from_secs(120),
            )
            .unwrap();
        drop(index);
        drop(session);

        // Corrupt the published generation's bytes in place (the envelope is
        // present, the payload is garbage).
        let gen_dir = index_root.join("generations").join(ws.raw().to_string());
        let published = std::fs::read_dir(&gen_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .expect("the published generation file exists");
        std::fs::write(&published, b"{ not a generation").unwrap();

        // A numeric workspace dir with no durable state row, plus junk the
        // doctor must ignore.
        std::fs::create_dir_all(index_root.join("generations").join("7777")).unwrap();
        std::fs::write(index_root.join("generations").join("not-a-workspace"), b"x").unwrap();
        std::fs::create_dir_all(index_root.join("generations").join("0")).unwrap();

        let report = doctor_run(dir.path(), false);
        assert!(
            report.lines.iter().any(|l| l.contains(&format!(
                "index embeddings: workspace {}: generation 1 unreadable",
                ws.raw()
            ))),
            "{:?}",
            report.lines
        );
        assert!(
            report.lines.iter().any(|l| l.contains(
                "index embeddings: workspace 7777: generation data exists with no durable index state"
            )),
            "{:?}",
            report.lines
        );
        assert!(
            !report.lines.iter().any(|l| l.contains("workspace 0:")),
            "the reserved id 0 must never be probed: {:?}",
            report.lines
        );
        assert!(
            !report.lines.iter().any(|l| l.contains("not-a-workspace")),
            "{:?}",
            report.lines
        );
        assert!(report.issues >= 2, "{:?}", report.lines);
    }

    /// The workspace probe budget is a bound, not a silent truncation:
    /// generation directories past it are counted, named as unprobed, and
    /// the run fails loudly.
    #[test]
    fn doctor_bounds_the_index_workspace_probe_budget_and_names_the_excess() {
        let dir = tempfile::tempdir().unwrap();
        let generations = dir
            .path()
            .join("store")
            .join("index_data")
            .join("generations");
        for raw in 1..=33u64 {
            std::fs::create_dir_all(generations.join(raw.to_string())).unwrap();
        }
        let report = doctor_run(dir.path(), false);
        assert!(
            report.lines.iter().any(|l| l
                .contains("index embeddings: 33 workspace(s) with persisted index generations")),
            "{:?}",
            report.lines
        );
        assert!(
            report.lines.iter().any(|l| l
                .contains("1 workspace(s) beyond the 32-workspace probe budget were NOT probed")),
            "{:?}",
            report.lines
        );
        assert!(report.issues >= 33, "{:?}", report.lines);
    }

    #[test]
    fn doctor_deep_flags_a_corrupt_cas_blob_and_never_silently_heals() {
        // Audit 73/74: doctor --deep lists a corrupted CAS blob as an issue
        // and a SECOND run finds the SAME issue — corruption is surfaced,
        // never repaired.
        let dir = tempfile::tempdir().unwrap();
        let hash_hex = {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let ws = session.create_workspace("/w").unwrap();
            let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
            let cas = session.cas();
            let hash = cas.put(b"doctor blob").unwrap();
            session
                .store()
                .put_artifact(sid, "command_output", &hash.to_hex(), "sum", 11)
                .unwrap();
            // Corrupt the blob behind the CAS's back: present file, wrong
            // content (not zstd, so verify_integrity flags it).
            let blob = cas.root().join(hash.cas_path());
            std::fs::write(&blob, b"this is not zstd-compressed content").unwrap();
            hash.to_hex()
        };
        let first = doctor_run(dir.path(), true);
        assert!(first.issues > 0, "{:?}", first.lines);
        assert!(
            first
                .lines
                .iter()
                .any(|l| l.contains("cas blob corrupt") && l.contains(&hash_hex)),
            "{:?}",
            first.lines
        );
        // Second run: still failing — no silent healing.
        let second = doctor_run(dir.path(), true);
        assert!(second.issues > 0, "{:?}", second.lines);
        assert!(
            second.lines.iter().any(|l| l.contains(&hash_hex)),
            "{:?}",
            second.lines
        );
    }

    #[test]
    fn doctor_deep_flags_dangling_and_malformed_cas_references() {
        let dir = tempfile::tempdir().unwrap();
        {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let ws = session.create_workspace("/w").unwrap();
            let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
            let missing = "ab".repeat(32);
            session
                .store()
                .put_artifact(sid, "command_output", &missing, "sum", 10)
                .unwrap();
            session
                .store()
                .put_artifact(sid, "command_output", "not-a-hex-hash", "sum", 10)
                .unwrap();
            let real = session.cas().put(b"present").unwrap();
            session
                .store()
                .put_artifact(sid, "command_output", &real.to_hex(), "sum", 7)
                .unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 2, "{:?}", report.lines);
        assert!(
            report.lines.iter().any(|l| {
                l.contains("dangling cas reference")
                    && l.contains(&"ab".repeat(32))
                    && l.contains("missing CAS blob")
            }),
            "{:?}",
            report.lines
        );
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("malformed CAS hash")),
            "{:?}",
            report.lines
        );
        // Rerun: the same refs are still dangling (no repair happened).
        let again = doctor_run(dir.path(), true);
        assert!(again.issues >= 2, "{:?}", again.lines);
    }

    #[test]
    fn doctor_deep_reports_global_running_rows_without_failing() {
        // Deep doctor surfaces cross-session recovery rows as INFORMATION
        // (a live daemon legitimately has running rows) — zero issues on an
        // otherwise healthy store.
        let dir = tempfile::tempdir().unwrap();
        {
            let session =
                SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                    .unwrap();
            let ws = session.create_workspace("/w").unwrap();
            let sid = session.create_session(ws, "t", "p", "m").unwrap().id();
            session
                .store()
                .start_tool_run(
                    sid,
                    faktor_core::id::OpId::new(7),
                    "echo",
                    serde_json::json!({}),
                    serde_json::json!({"strategy": "none"}),
                    None,
                    None,
                )
                .unwrap();
            // The active turn's durable anchor: admission materializes the
            // prompt message at the PromptReceived journal seq, and the turn
            // record names that seq. Without the anchor the turn would be an
            // unrecoverable active turn (nothing could own it after a crash).
            session
                .store()
                .put_message(sid, 2, "user", serde_json::json!({"text": "x"}))
                .unwrap();
            session
                .store()
                .start_turn_record(
                    sid,
                    faktor_core::id::OpId::new(9),
                    None,
                    Some(2),
                    "p",
                    "m",
                    None,
                )
                .unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert_eq!(report.issues, 0, "{:?}", report.lines);
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("running tool runs across all sessions: 1")),
            "{:?}",
            report.lines
        );
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("active logical turns across all sessions: 1")),
            "{:?}",
            report.lines
        );
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("active turns with recoverable owners: 1 of 1")),
            "{:?}",
            report.lines
        );
    }

    // ---------------------------------- doctor deep invariants (P0-97/74/100)

    /// Drive one REAL task to `VerifiedComplete` through the session APIs —
    /// the ONLY legal path — with a live OPEN reservation on its row. Used
    /// by every corruption test as the healthy baseline.
    fn seed_verified_complete(
        m: &Arc<SessionManager>,
    ) -> (faktor_core::id::SessionId, faktor_core::id::TaskId, i64) {
        use faktor_core::id::{SessionId, TaskId};
        use faktor_core::state::{
            CriterionVerification, TaskState, TaskTransition, VerificationStatus,
        };
        let ws = m.create_workspace("/w").unwrap();
        let s = m.create_session(ws, "t", "p", "m").unwrap();
        let sid: SessionId = s.id();
        let task_id = TaskId::new(42);
        let now = m.now_ms();
        s.create_task(faktor_session::Task {
            task_id,
            session_id: sid,
            goal: "make it so".into(),
            acceptance_criteria: vec!["c1".into()],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: TaskState::Running,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
        let r1 = s.task_revision(task_id).unwrap();
        s.transition_task(task_id, r1, TaskTransition::RequestVerification, None)
            .unwrap();
        let r2 = s.task_revision(task_id).unwrap();
        s.transition_task(task_id, r2, TaskTransition::StartVerification, None)
            .unwrap();
        let r3 = s.task_revision(task_id).unwrap();
        let record = s
            .create_verification_record(
                task_id,
                None,
                vec![CriterionVerification {
                    criterion_key: "c1".into(),
                    passed: true,
                    evidence: None,
                    binding: None,
                }],
                vec![],
                vec![],
                vec![],
                None,
                VerificationStatus::Passed,
                now,
            )
            .unwrap();
        s.complete_verified_task(task_id, r3, record).unwrap();
        // A live open reservation against a REAL task row in a
        // provider-permitting state: a VerifiedComplete task forbids new
        // provider operations (and a completion cannot carry an open
        // reservation), so the healthy baseline parks the reservation on a
        // second Running task of the same session.
        let reservation_task = TaskId::new(43);
        s.create_task(faktor_session::Task {
            task_id: reservation_task,
            session_id: sid,
            goal: "hold a live reservation".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: TaskState::Running,
            created_ms: m.now_ms(),
            updated_ms: m.now_ms(),
        })
        .unwrap();
        m.store()
            .cost_task_cap_set(sid, reservation_task, Some(1_000_000))
            .unwrap();
        let op = m.try_next_op_id().unwrap();
        let granted = m
            .store()
            .cost_reserve(sid, reservation_task, op, 1000, now)
            .unwrap();
        let reservation_id = match granted {
            faktor_store::CostReserveOutcome::Granted(id) => id,
            _ => panic!("reservation must be granted"),
        };
        (sid, task_id, reservation_id)
    }

    /// Raw sqlite handle for crafting corruption AFTER the manager closed
    /// (doctor reopens the same file afterwards). The db lives at
    /// `store/faktor-plus.db` under the doctor data dir.
    fn raw_corruption_conn(data_dir: &std::path::Path) -> rusqlite::Connection {
        rusqlite::Connection::open(data_dir.join("store").join("faktor-plus.db")).unwrap()
    }

    #[test]
    fn doctor_deep_passes_every_p097_section_on_a_healthy_store() {
        // The full audit surface passes on data produced ONLY through the
        // real APIs: a VerifiedComplete task + its Passed record, a live
        // reservation on the real task row, an orchestrated child identity
        // row + one well-formed non-terminal registry row whose worktree
        // directory exists.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let parent = m.create_session(ws, "parent", "p", "m").unwrap();
            let (sid, task_id, _reservation) = seed_verified_complete(&m);
            let wt_path = dir.path().join("wt");
            std::fs::create_dir_all(&wt_path).unwrap();
            let wt_id = m
                .put_worktree(ws, wt_path.to_str().unwrap(), "feat/x")
                .unwrap();
            let child = m
                .create_child_session(
                    parent.id(),
                    ws,
                    faktor_core::WorktreeId::new(wt_id as u64),
                    faktor_core::TaskId::new(42),
                    "p",
                    "m",
                    "child",
                    faktor_session::ChildOwnership::ReadOnlyShared,
                )
                .unwrap();
            assert_eq!(parent.id().raw(), 1, "parent session id");
            assert_eq!(sid.raw(), 2, "verified task's session id");
            assert_eq!(child.id().raw(), 3, "child session id");
            // A well-formed NON-terminal registry row over the child (the
            // executor's durable shape, parent row space), whose worktree
            // dir exists.
            let runtime = faktor_orchestrator::runtime::ChildRuntime {
                child_id: "child-3".into(),
                parent_session_id: parent.id().raw(),
                run_id: "run-1".into(),
                item_id: "w1".into(),
                kind: faktor_orchestrator::WorkKind::Exploration,
                session_id: child.id().raw(),
                operation_id: 0,
                workspace_id: ws.raw(),
                worktree_id: wt_id as u64,
                ownership: faktor_session::ChildOwnership::ReadOnlyShared,
                ownership_paths: vec![],
                state: faktor_orchestrator::ChildState::Running,
                budget_max_tokens: None,
                permissions: faktor_orchestrator::caps::CapabilitySet::default(),
                model_policy: faktor_orchestrator::runtime::ModelPolicy::default(),
                blocker_kind: None,
                blocker_reason: None,
                blocker_dependency: None,
                blocker_resolution: None,
                last_progress_ms: None,
                execution_phase: faktor_orchestrator::runtime::ExecutionPhase::default(),
                created_ms: 1,
                updated_ms: 1,
                base_snapshot_id: None,
                run_base_snapshot: None,
                env_snapshot_id: None,
            };
            let value = serde_json::to_string(&runtime).unwrap();
            parent
                .upsert_memory_fact(
                    "orchestrator_registry",
                    &format!("run-1/{}", runtime.child_id),
                    &value,
                )
                .unwrap();
            // Keep the (unused) id referenced so the data stays typed.
            let _ = task_id;
        }
        let report = doctor_run(dir.path(), true);
        assert_eq!(report.issues, 0, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            report
                .lines
                .iter()
                .any(|l| l == "cost reservations: 1 (open 1, settled 0, refunded 0, uncertain 0)"),
            "{text}"
        );
        assert!(text.contains("dangling cost reservations: none"), "{text}");
        assert!(
            text.contains("verification records: 1 record(s), 1 completion-relevant task(s), 1 VerifiedComplete task(s)"),
            "{text}"
        );
        assert!(text.contains("verification consistency: ok"), "{text}");
        assert!(text.contains("journal consistency: ok"), "{text}");
        assert!(
            text.contains("orphan children: 1 child identity row(s), 1 registry row(s) scanned"),
            "{text}"
        );
        assert!(text.contains("orphan children: none"), "{text}");
        assert!(
            text.contains("active turns with recoverable owners: 0 of 0"),
            "{text}"
        );
        assert!(
            text.contains("process ownership: 0 durable session-owned process row(s)"),
            "{text}"
        );
        // None of the failing prefixes may appear.
        for bad in [
            "dangling cost reservation:",
            "verification inconsistency",
            "orphan child:",
            "active turn without recoverable owner",
        ] {
            assert!(!text.contains(bad), "{bad} present in: {text}");
        }
    }

    #[test]
    fn doctor_deep_flags_dangling_open_and_settled_cost_reservations() {
        // Raw insert: RESERVED + SETTLED + UNCERTAIN reservation rows whose
        // task row does not exist (no store API can produce them —
        // cost_reserve refuses a missing task). Deep doctor must report the
        // typed section with the per-status count (reserved folds into the
        // in-flight "open" bucket) and one failing line per dangling row.
        // The legacy 'open'/'abandoned' vocabulary is dead: the v17 schema
        // CHECK rejects them at insert.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            m.create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
                .unwrap();
        }
        {
            let conn = raw_corruption_conn(dir.path());
            conn.execute(
                "INSERT INTO cost_reservation(session_id, task_id, op_id, predicted_micro, status, created_ms)
                 VALUES (1, 424242, 5, 1234, 'reserved', 1),
                        (1, 424243, 6, 999, 'settled', 1),
                        (1, 424244, 7, 100, 'uncertain', 1)",
                [],
            )
            .unwrap();
            for dead in ["open", "abandoned"] {
                let legacy = conn.execute(
                    "INSERT INTO cost_reservation(session_id, task_id, op_id, predicted_micro, status, created_ms)
                     VALUES (1, 424245, 8, 50, ?, 1)",
                    [dead],
                );
                assert!(
                    legacy.is_err(),
                    "the v17 CHECK forbids the legacy {dead:?} vocabulary"
                );
            }
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 3, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            text.contains("cost reservations: 3 (open 1, settled 1, refunded 0, uncertain 1)"),
            "{text}"
        );
        assert!(
            report.lines.iter().any(|l| {
                l.contains("dangling cost reservation: reservation 1")
                    && l.contains("task 424242")
                    && l.contains("status reserved")
            }),
            "{text}"
        );
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("task 424243") && l.contains("status settled")),
            "{text}"
        );
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("task 424244") && l.contains("status uncertain")),
            "{text}"
        );
    }

    #[test]
    fn doctor_deep_flags_a_passed_record_for_a_missing_task() {
        // The store's raw record insert is deliberately unvalidated (the
        // session layer is the guard) — so a Passed record can reference a
        // task that never existed. Deep doctor must FAIL the wave-16
        // section with the exact kind and counts.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            let _s = m
                .create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
                .unwrap();
            let row = faktor_store::VerificationRecordRow {
                id: faktor_core::id::VerificationRecordId::new(1), // ignored
                task_id: faktor_core::id::TaskId::new(777),
                revision: faktor_core::id::TaskRevision::new(1),
                workspace_id: faktor_core::id::WorkspaceId::new(1),
                worktree_id: faktor_core::id::WorktreeId::new(1),
                tree_hash: None,
                criteria: vec![],
                checks: vec![],
                changed_files: vec![],
                unrelated_changes: vec![],
                reviewer: None,
                status: faktor_core::state::VerificationStatus::Passed,
                started_ms: 1,
                completed_ms: None,
            };
            m.store().verification_record_put(&row).unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 1, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            report.lines.iter().any(|l| {
                l.contains("verification inconsistency [record_without_task]")
                    && l.contains("task 777")
            }),
            "{text}"
        );
        assert!(
            text.contains("verification records: 1 record(s), 0 completion-relevant task(s), 0 VerifiedComplete task(s)"),
            "{text}"
        );
    }

    #[test]
    fn doctor_deep_flags_passed_record_certifying_an_uncompleted_task() {
        // A Passed record may only certify the CURRENT revision of a
        // VerifiedComplete task. Raw-putting one against a Running task at
        // its revision is the exact wave-16 bypass deep doctor must catch.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let s = m.create_session(ws, "t", "p", "m").unwrap();
            let task_id = faktor_core::id::TaskId::new(9);
            let now = m.now_ms();
            s.create_task(faktor_session::Task {
                task_id,
                session_id: s.id(),
                goal: "g".into(),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: Vec::new(),
                budget: faktor_session::TaskBudget::default(),
                state: faktor_core::state::TaskState::Running,
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
            let row = faktor_store::VerificationRecordRow {
                id: faktor_core::id::VerificationRecordId::new(1), // ignored
                task_id,
                revision: faktor_core::id::TaskRevision::new(1),
                workspace_id: ws,
                worktree_id: faktor_core::id::WorktreeId::new(1),
                tree_hash: None,
                criteria: vec![],
                checks: vec![],
                changed_files: vec![],
                unrelated_changes: vec![],
                reviewer: None,
                status: faktor_core::state::VerificationStatus::Passed,
                started_ms: 1,
                completed_ms: None,
            };
            m.store().verification_record_put(&row).unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 1, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            report.lines.iter().any(|l| {
                l.contains("verification inconsistency [passed_on_uncompleted]")
                    && l.contains("task 1/9")
                    && l.contains("revision 1")
            }),
            "{text}"
        );
    }

    #[test]
    fn doctor_deep_flags_verified_complete_task_whose_record_was_deleted() {
        // A legitimately completed task first passes every section; deleting
        // its consumed Passed record (raw SQL) must make the same dir FAIL
        // the wave-16 section — VerifiedComplete without its completion
        // proof.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            seed_verified_complete(&m);
        }
        let clean = doctor_run(dir.path(), true);
        assert_eq!(clean.issues, 0, "{:?}", clean.lines);
        {
            let conn = raw_corruption_conn(dir.path());
            conn.execute("DELETE FROM verification_record", []).unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 1, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            report.lines.iter().any(|l| {
                l.contains("verification inconsistency [verified_without_record]")
                    && l.contains("task 1/42")
                    && l.contains("at revision 4")
                    && l.contains("completion revision 3")
            }),
            "{text}"
        );
        assert!(
            text.contains("verification records: 0 record(s), 1 completion-relevant task(s), 1 VerifiedComplete task(s)"),
            "{text}"
        );
        assert!(
            text.contains("cost reservations: 1 (open 1, settled 0, refunded 0, uncertain 0)"),
            "{text}"
        );
    }

    #[test]
    fn doctor_deep_flags_active_turn_without_recoverable_owner() {
        // Raw insert of an active turn_record with NO durable anchor (no
        // prompt message, no queue row, no journal event, no tool-run row
        // names its op): after a crash nothing could own this turn.
        let dir = tempfile::tempdir().unwrap();
        {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            m.create_session(m.create_workspace("/w").unwrap(), "t", "p", "m")
                .unwrap();
        }
        {
            let conn = raw_corruption_conn(dir.path());
            conn.execute(
                "INSERT INTO turn_record(session_id, turn_op_id, started_at, status, updated_ms)
                 VALUES (1, 999999, 1, 'active', 1)",
                [],
            )
            .unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 1, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            text.contains("active turns with recoverable owners: 0 of 1"),
            "{text}"
        );
        assert!(
            report.lines.iter().any(|l| {
                l.contains("active turn without recoverable owner") && l.contains("op 999999")
            }),
            "{text}"
        );
    }

    #[test]
    fn doctor_deep_flags_orphan_child_rows_and_a_missing_worktree_dir() {
        // Three orphan-child corruptions: an unparseable registry row, a
        // child identity row naming a parent session that does not exist,
        // and a well-formed NON-terminal registry row whose worktree
        // directory vanished from disk.
        let dir = tempfile::tempdir().unwrap();
        let wt_row_id = {
            let m = SessionManager::open_quick(dir.path().join("store"), dir.path().join("cas"))
                .unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let parent = m.create_session(ws, "parent", "p", "m").unwrap();
            let _child = m.create_session(ws, "child", "p", "m").unwrap();
            let wt_path = dir.path().join("vanished-wt");
            std::fs::create_dir_all(&wt_path).unwrap();
            let wt_id = m
                .put_worktree(ws, wt_path.to_str().unwrap(), "feat/x")
                .unwrap();
            // A well-formed NON-terminal registry row (child session 2 is
            // live; the worktree DIRECTORY is removed below).
            let runtime = faktor_orchestrator::runtime::ChildRuntime {
                child_id: "child-2".into(),
                parent_session_id: parent.id().raw(),
                run_id: "run-1".into(),
                item_id: "w1".into(),
                kind: faktor_orchestrator::WorkKind::Exploration,
                session_id: 2,
                operation_id: 0,
                workspace_id: ws.raw(),
                worktree_id: wt_id as u64,
                ownership: faktor_session::ChildOwnership::ReadOnlyShared,
                ownership_paths: vec![],
                state: faktor_orchestrator::ChildState::Running,
                budget_max_tokens: None,
                permissions: faktor_orchestrator::caps::CapabilitySet::default(),
                model_policy: faktor_orchestrator::runtime::ModelPolicy::default(),
                blocker_kind: None,
                blocker_reason: None,
                blocker_dependency: None,
                blocker_resolution: None,
                last_progress_ms: None,
                execution_phase: faktor_orchestrator::runtime::ExecutionPhase::default(),
                created_ms: 1,
                updated_ms: 1,
                base_snapshot_id: None,
                run_base_snapshot: None,
                env_snapshot_id: None,
            };
            let value = serde_json::to_string(&runtime).unwrap();
            parent
                .upsert_memory_fact(
                    "orchestrator_registry",
                    &format!("run-1/{}", runtime.child_id),
                    &value,
                )
                .unwrap();
            std::fs::remove_dir_all(&wt_path).unwrap();
            wt_id
        };
        {
            let conn = raw_corruption_conn(dir.path());
            // Unparseable registry row under session 1 (corruption, never a
            // silent skip).
            conn.execute(
                "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
                 VALUES (1, 'orchestrator_registry', 'run-1/child-x', '{not-json', 1)",
                [],
            )
            .unwrap();
            // Child identity naming a parent session that has no row.
            conn.execute(
                "INSERT INTO memory_fact(session_id, kind, key, value, updated_ms)
                 VALUES (2, 'orchestrator', 'identity',
                         '{\"parent_session_id\":777,\"workspace_id\":1,\"worktree_id\":1,\"item_id\":\"i\",\"task_goal\":\"\",\"operation_id\":0,\"ownership\":\"read_only_shared\",\"model\":\"\",\"created_ms\":1}',
                         1)",
                [],
            )
            .unwrap();
        }
        let report = doctor_run(dir.path(), true);
        assert!(report.issues >= 3, "{:?}", report.lines);
        let text = report.lines.join("\n");
        assert!(
            report
                .lines
                .iter()
                .any(|l| l.contains("orphan child: unparseable orchestrator registry row")),
            "{text}"
        );
        assert!(
            report.lines.iter().any(|l| {
                l.contains("orphan child: child session 2 carries an identity row naming parent session 777")
            }),
            "{text}"
        );
        assert!(
            report.lines.iter().any(|l| {
                l.contains("orphan child: non-terminal child child-2")
                    && l.contains("no worktree directory")
            }),
            "{text}"
        );
        assert!(
            text.contains("orphan children: 1 child identity row(s), 2 registry row(s) scanned"),
            "{text}"
        );
        let _ = wt_row_id;
    }

    // ------------------------------------------------------------- wiring

    /// One chat request whose user text can echo configured secrets.
    fn chat_req(text: &str) -> GenericAgentRequest {
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
    async fn chat_text(provider: Arc<dyn Provider>, text: &str) -> Result<String, ProviderError> {
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
    fn egress_cfg(
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_secret_registry_blocks_a_configured_key_echo_before_connect() {
        const KEY_ENV: &str = "FAKTOR_TEST_CLI_WIRING_FAKE_KEY";
        const SECRET: &str = "kp-cli-secret-token-91f7c2e8d4";
        // The value must not trip the frozen GENERIC scan patterns (sk-*,
        // ghp_, AKIA, ...): only the configured-secret registry can catch
        // it, so a block proves the registry was populated from the key.
        std::env::set_var(KEY_ENV, SECRET);
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: sse_body(&[serde_json::json!({
                    "choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]
                })]),
            },
        );
        let base = server.base_url().await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let cfg = egress_cfg(
            dir.path(),
            "secret.json",
            &base,
            Some(KEY_ENV),
            Some(&[format!("http://127.0.0.1:{port}")]),
        );
        let graph = build_daemon(dir.path(), Some(cfg)).unwrap();
        let provider = graph.providers.get("mocked").unwrap();
        // Control: a clean body reaches the mock (the scan does not
        // false-positive on ordinary chat text).
        let text = chat_text(provider.clone(), "hello").await.unwrap();
        assert_eq!(text, "ok");
        assert_eq!(server.request_count(), 1);
        // The key echo: the whole payload is scanned and the request is
        // denied BEFORE any connect — the mock never sees it.
        let err = chat_text(provider, SECRET)
            .await
            .expect_err("a configured key echoed in the body must be blocked");
        assert!(err.message.contains("secret"), "{}", err.message);
        assert!(!err.retryable);
        assert_eq!(
            server.request_count(),
            1,
            "the key-echo request never arrived at the server"
        );
        drop(graph);
        std::env::remove_var(KEY_ENV);
    }

    #[test]
    fn verification_config_unknown_fields_fail_and_zero_disables_the_service() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v.json");
        // Sane values parse (strictly) and drive the daemon service: the
        // derived per-check budget is the configured cap.
        std::fs::write(
            &path,
            r#"{"verification": {"quick_max_s": 30, "unit_max_s": 120, "full_as_background": false}}"#,
        )
        .unwrap();
        let cfg = serve_config(Some(path.clone())).expect("explicit sane config");
        let supervisor = ProcessSupervisor::try_shared().expect("standalone supervisor");
        let service = daemon_verification(&cfg.verification, &supervisor);
        assert!(!service.is_disabled());
        let policy = service.policy();
        assert_eq!(policy.quick_max, std::time::Duration::from_secs(30));
        assert_eq!(policy.unit_max, std::time::Duration::from_secs(120));
        assert!(!policy.full_as_background);
        // Budget probe through the service (same decision the genuine-end
        // site applies per check category).
        let quick = faktor_verify::exec::CheckSpec::new(
            "q",
            faktor_verify::exec::CheckKind::Compile,
            faktor_verify::exec::CheckCategory::Quick,
            "cargo",
            ["check"],
            true,
        );
        assert_eq!(
            service.budget_for(&quick),
            faktor_verify::exec::BudgetDecision::RunInline(std::time::Duration::from_secs(30))
        );
        // An explicit unknown field inside [verification] fails startup.
        std::fs::write(&path, r#"{"verification": {"bogus": 1}}"#).unwrap();
        let e = serve_config(Some(path.clone())).expect_err("unknown field fails startup");
        assert!(e.contains("unknown field"), "{e}");
        // quick_max_s = 0 yields the DISABLED service: fail closed.
        std::fs::write(
            &path,
            r#"{"verification": {"quick_max_s": 0, "unit_max_s": 0}}"#,
        )
        .unwrap();
        let cfg = serve_config(Some(path)).expect("zero quick budget is a valid config");
        let supervisor = ProcessSupervisor::try_shared().expect("standalone supervisor");
        let service = daemon_verification(&cfg.verification, &supervisor);
        assert!(
            service.is_disabled(),
            "quick_max_s = 0 must disable verification (fail closed)"
        );
    }

    #[test]
    fn network_guarantee_required_flows_into_the_daemon_sandbox_gate() {
        // Authority direction (audit P0-39): the configured guarantee
        // reaches the daemon's PermissionEngine, which DECIDES the spawn
        // requirement (Required => DenyAll) and performs NO platform
        // pre-judgement. The capability verdict stays the plain rule (Ask
        // by default); enforcement honesty belongs to the spawn layer,
        // which refuses typed when it cannot isolate.
        let cfg = config::Config {
            sandbox: config::SandboxCfg {
                network_guarantee: faktor_sandbox::SandboxGuarantee::Required,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            cfg.sandbox_policy().unwrap().network_guarantee,
            faktor_sandbox::SandboxGuarantee::Required
        );
        let dir = tempfile::tempdir().unwrap();
        let graph = build_daemon(dir.path(), Some(cfg)).unwrap();
        let sandbox = graph
            .agent
            .deps()
            .sandbox
            .clone()
            .expect("daemon sandbox wired");
        assert_eq!(
            sandbox.policy().network_guarantee,
            faktor_sandbox::SandboxGuarantee::Required,
            "the guarantee surfaces into the daemon SandboxPolicy"
        );
        assert_eq!(
            sandbox.spawn_network_requirement(),
            faktor_terminal::NetworkIsolationRequirement::DenyAll,
            "Required must reach the spawn seam as DenyAll"
        );
        let decision = sandbox.evaluate(&Capability::ExecuteShell {
            command: "echo hi".into(),
        });
        assert_eq!(
            decision,
            faktor_core::PermissionDecision::Ask,
            "no preflight platform guessing: the rule decides (Ask default)"
        );
        drop(graph);
    }

    #[test]
    fn daemon_hooks_run_env_clear_exact_through_the_daemon_supervisor() {
        // (b) cli-level hook harness: the daemon hook registry constructor
        // (supervisor-rooted, daemon envelope) runs a hook whose env is
        // cleared EXACTLY (only explicit entries + FAKTOR_HOOK_INPUT), and
        // the audit row lands with the bounded stdout.
        std::env::set_var("FAKTOR_TEST_DAEMON_ONLY_SECRET", "must-not-leak-to-hooks");
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let supervisor = ProcessSupervisor::new(session.cas());
        let spec = faktor_hooks::HookSpec {
            id: "env-0".into(),
            events: vec![faktor_hooks::HookEvent::PreTool],
            command: "/usr/bin/env".into(),
            args: vec![],
            env: vec![("FAKTOR_TEST_HOOK_VISIBLE".into(), "visible".into())],
            env_allowlist: true,
            deadline_ms: 5000,
            failure_policy: faktor_hooks::FailurePolicy::FailClosed,
            ..Default::default()
        };
        let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
        let verdict = registry.run(
            faktor_hooks::HookEvent::PreTool,
            &faktor_hooks::HookInput::default(),
        );
        assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
        let audit = registry.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].hook_id, "env-0");
        let stdout = &audit[0].stdout_head;
        assert!(
            stdout.contains("FAKTOR_TEST_HOOK_VISIBLE=visible"),
            "explicit entries pass: {stdout}"
        );
        assert!(
            stdout.contains("FAKTOR_HOOK_INPUT="),
            "the input JSON rides the env: {stdout}"
        );
        assert!(
            !stdout.contains("FAKTOR_TEST_DAEMON_ONLY_SECRET"),
            "env-clear exact: the daemon env must never reach the hook: {stdout}"
        );
        // The run went through the supervisor registry and left nothing
        // alive (bounded child, reaped).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !supervisor.alive().is_empty() {
            assert!(std::time::Instant::now() < deadline, "child leaked");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::env::remove_var("FAKTOR_TEST_DAEMON_ONLY_SECRET");
    }

    #[test]
    fn daemon_hook_deadline_kills_the_group_and_audits_the_refusal() {
        // (b) deadline group-kill through the daemon supervisor: an
        // over-deadline hook is killed process-group-wide, its audit row
        // records the refusal (never the partial output), and no child is
        // left behind.
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let supervisor = ProcessSupervisor::new(session.cas());
        let spec = faktor_hooks::HookSpec {
            id: "slow".into(),
            events: vec![faktor_hooks::HookEvent::PreTool],
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env_allowlist: true,
            deadline_ms: 300,
            failure_policy: faktor_hooks::FailurePolicy::FailClosed,
            ..Default::default()
        };
        let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
        let verdict = registry.run(
            faktor_hooks::HookEvent::PreTool,
            &faktor_hooks::HookInput::default(),
        );
        assert!(
            matches!(verdict, faktor_hooks::HookVerdict::Deny { .. }),
            "fail-closed deadline refusal: {verdict:?}"
        );
        let audit = registry.audit();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].hook_id, "slow");
        assert_eq!(audit[0].verdict, "deny");
        assert!(
            audit[0].duration_ms >= 200,
            "the deadline dominated: {} ms",
            audit[0].duration_ms
        );
        assert!(
            audit[0].exit_code.is_none() && audit[0].stdout_head.is_empty(),
            "killed before output; partial output never decides: {:?}",
            audit[0]
        );
        // Group-kill: no live child survives the deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !supervisor.alive().is_empty() {
            assert!(std::time::Instant::now() < deadline, "killed child leaked");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mcp_hook_and_terminal_children_share_one_daemon_supervisor() {
        // (a) The daemon owns EXACTLY ONE supervisor. Two MCP servers, a
        // long hook and a long terminal run CONCURRENTLY all admit into the
        // SAME bounded registry: while the hook and the terminal are live,
        // the single supervisor accounts 4 live children (2 mcp + hook +
        // terminal) — the old per-server supervisors would have split them
        // across registries.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 missing; skipping");
            return;
        }
        let fixture = format!("{}/tests/fixtures/mcp_mock.py", env!("CARGO_MANIFEST_DIR"));
        assert!(
            std::path::Path::new(&fixture).exists(),
            "mcp fixture missing at {fixture}"
        );
        let mcp_entries = vec![
            config::McpEntry {
                name: "mock-a".into(),
                command: "python3".into(),
                args: vec![fixture.clone()],
            },
            config::McpEntry {
                name: "mock-b".into(),
                command: "python3".into(),
                args: vec![fixture.clone()],
            },
        ];
        let cfg = config::Config {
            mcp: mcp_entries,
            ..Default::default()
        };
        // The env hook path registers NO long hook here (the env format
        // cannot carry an unquoted `sleep 3` argument vector), so the long
        // hook is registered through the SAME cli helper `serve` uses
        // (env_hook_registry delegates here) onto the daemon's supervisor.
        let dir = tempfile::tempdir().unwrap();
        let graph = build_daemon_with_mcp(dir.path(), Some(cfg))
            .await
            .expect("daemon with two mcp servers builds");
        let supervisor = graph
            .agent
            .deps()
            .supervisor
            .clone()
            .expect("daemon supervisor wired");
        let registry = hook_registry(
            &supervisor,
            vec![faktor_hooks::HookSpec {
                id: "env-0".into(),
                events: vec![faktor_hooks::HookEvent::PreTool],
                command: "/bin/sh".into(),
                args: vec!["-c".into(), "sleep 3".into()],
                env_allowlist: true,
                deadline_ms: 20_000,
                failure_policy: faktor_hooks::FailurePolicy::FailClosed,
                ..Default::default()
            }],
        )
        .expect("long hook registered");
        let hooks = registry;
        // Both MCP children live in the daemon supervisor's registry.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if supervisor.alive().len() == 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "two mcp children never appeared: {:?}",
                supervisor.alive()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(graph.mcp_servers.iter().all(|s| s.is_alive()));
        // A long hook AND a long terminal child concurrently with the MCP
        // servers: one registry counts all four.
        let registry = hooks.clone();
        let hook_thread = std::thread::spawn(move || {
            registry.run(
                faktor_hooks::HookEvent::PreTool,
                &faktor_hooks::HookInput::default(),
            )
        });
        let sup = supervisor.clone();
        let terminal_thread = std::thread::spawn(move || {
            sup.run_sync(
                SpawnConfig {
                    cmd: "/bin/sh".into(),
                    args: vec!["-c".into(), "sleep 3".into()],
                    cwd: std::env::temp_dir(),
                    env: EnvSpec::Minimal,
                    owner: ProcessOwner::Daemon,
                    ..Default::default()
                },
                std::time::Duration::from_secs(30),
                64 * 1024,
                64 * 1024,
            )
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if supervisor.alive().len() == 4 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "expected 4 live children in the ONE registry, saw {:?}",
                supervisor.alive()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let hook_verdict = hook_thread.join().expect("hook thread panicked");
        assert_eq!(
            hook_verdict,
            faktor_hooks::HookVerdict::Allow,
            "the long hook exits cleanly within its deadline"
        );
        terminal_thread
            .join()
            .expect("terminal thread panicked")
            .expect("the terminal child must complete");
        // Both finished: the MCP children remain, counted by the SAME
        // supervisor the hooks and terminal ran through.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if supervisor.alive().len() == 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "children leaked after the runs: {:?}",
                supervisor.alive()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        drop(graph);
    }

    #[test]
    fn full_scope_env_hooks_run_under_the_daemon_envelope() {
        // The daemon envelope grants the full capability lattice: an env
        // hook whose typed scope is the TOP element still registers and
        // runs (construction proof of the with_supervisor envelope seam).
        let dir = tempfile::tempdir().unwrap();
        let session =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let supervisor = ProcessSupervisor::new(session.cas());
        let spec = faktor_hooks::HookSpec {
            id: "full".into(),
            events: vec![faktor_hooks::HookEvent::TaskComplete],
            command: "/bin/echo".into(),
            args: vec!["done".into()],
            permission_scope: CapabilitySet::ALL,
            ..Default::default()
        };
        let registry = hook_registry(&supervisor, vec![spec]).expect("registry built");
        let verdict = registry.run(
            faktor_hooks::HookEvent::TaskComplete,
            &faktor_hooks::HookInput::default(),
        );
        assert_eq!(verdict, faktor_hooks::HookVerdict::Allow);
        assert_eq!(registry.audit().len(), 1);
    }

    // ------------- daemon ACP terminal round-trip (the REAL authority) -----

    /// Byte-level ACP client over the client half of an in-memory duplex.
    #[cfg(unix)]
    struct AcpWire {
        read: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        buf: Vec<u8>,
        next_id: u64,
    }

    #[cfg(unix)]
    impl AcpWire {
        async fn expect_message(&mut self) -> Value {
            use tokio::io::AsyncReadExt;
            loop {
                match faktor_acp::protocol::parse_frame(&self.buf) {
                    Ok(Some((consumed, value))) => {
                        self.buf.drain(..consumed);
                        return value;
                    }
                    Ok(None) => {
                        let mut chunk = [0u8; 8192];
                        let n = tokio::time::timeout(
                            std::time::Duration::from_secs(20),
                            self.read.read(&mut chunk),
                        )
                        .await
                        .expect("server answers within the test bound")
                        .expect("server read");
                        assert!(n > 0, "server closed the pipe before answering");
                        self.buf.extend_from_slice(&chunk[..n]);
                    }
                    Err(e) => panic!("client framing error: {e}"),
                }
            }
        }

        async fn send(&mut self, method: &str, params: Value) -> u64 {
            use tokio::io::AsyncWriteExt;
            let id = self.next_id;
            self.next_id += 1;
            let bytes = faktor_acp::protocol::frame(method.to_string(), id, params);
            self.write.write_all(&bytes).await.expect("client write");
            self.write.flush().await.expect("client flush");
            id
        }

        async fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.send(method, params).await;
            loop {
                let msg = self.expect_message().await;
                let is_response = msg.get("result").is_some() || msg.get("error").is_some();
                if msg["id"] == json!(id) && is_response {
                    return msg;
                }
            }
        }

        /// Send one request and read until BOTH its response and a
        /// `terminalOutput` frame whose data contains `needle` have arrived
        /// (the two race, and neither may be dropped).
        async fn request_until_output(
            &mut self,
            method: &str,
            params: Value,
            terminal_id: &str,
            needle: &str,
        ) -> (Value, Vec<String>) {
            let id = self.send(method, params).await;
            let mut updates: Vec<String> = Vec::new();
            let mut response: Option<Value> = None;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            loop {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for terminal output {needle:?}"
                );
                let msg = self.expect_message().await;
                if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                    let update = &msg["params"]["update"];
                    if update["kind"] == "terminalOutput"
                        && update["terminalId"] == json!(terminal_id)
                    {
                        updates.push(update["data"].as_str().unwrap_or_default().to_string());
                    }
                } else if msg["id"] == json!(id) {
                    response = Some(msg);
                }
                if updates.iter().any(|data| data.contains(needle)) {
                    if let Some(response) = response {
                        return (response, updates);
                    }
                }
            }
        }
    }

    /// Spawn the ACP server over a duplex; returns the client plus the serve
    /// task. `authority` is attached when present (the production wiring).
    #[cfg(unix)]
    fn start_daemon_terminal_server(
        backend: DaemonAcpBackend,
        authority: Option<Arc<dyn faktor_acp::TerminalAuthority>>,
    ) -> (AcpWire, tokio::task::JoinHandle<Result<(), String>>) {
        let server = match authority {
            Some(authority) => AcpServer::new(backend).with_terminal_authority(authority),
            None => AcpServer::new(backend),
        };
        let (server_side, client_side) = tokio::io::duplex(1024 * 1024);
        let (server_r, server_w) = tokio::io::split(server_side);
        let (client_r, client_w) = tokio::io::split(client_side);
        let task = tokio::spawn(async move { server.serve_connection(server_r, server_w).await });
        (
            AcpWire {
                read: client_r,
                write: client_w,
                buf: Vec::new(),
                next_id: 1,
            },
            task,
        )
    }

    /// Create one REAL durable session through the ACP wire, returning its id.
    #[cfg(unix)]
    async fn acp_new_real_session(client: &mut AcpWire, workspace: &std::path::Path) -> String {
        std::fs::create_dir_all(workspace).unwrap();
        let new = client
            .request(
                "session/new",
                json!({ "workspace": workspace.to_str().unwrap() }),
            )
            .await;
        assert_eq!(new["error"], Value::Null, "session/new failed: {new}");
        new["result"]["sessionId"]
            .as_str()
            .expect("sessionId")
            .to_string()
    }

    /// Native liveness probe of one raw pid. `kill(pid, 0) == 0` means the
    /// pid exists (a live child OR an unreaped zombie); `EPERM` also means it
    /// exists — the pid is real but not ours to signal, and on Darwin a group
    /// of unreaped zombies answers `EPERM`. Only `ESRCH` proves the pid is
    /// gone. No shell subprocess: a transient spawn failure under full-suite
    /// load (`EAGAIN`/`ENOMEM`) can never be misread as "the child died", and
    /// the probe matches the wait's observable state exactly.
    #[cfg(unix)]
    #[allow(unsafe_code)]
    fn pid_probe(pid: u32) -> Result<(), i32> {
        if pid == 0 {
            return Err(libc::ESRCH);
        }
        // SAFETY: signal 0 only probes existence and never delivers a signal;
        // the pid fits `pid_t` and a recycled/invalid id can at worst report
        // the wrong liveness, never crash the probe.
        let r = unsafe { libc::kill(pid as i32, 0) };
        if r == 0 {
            return Ok(());
        }
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if errno == libc::EPERM {
            Ok(())
        } else {
            Err(errno)
        }
    }

    #[cfg(unix)]
    fn pid_alive(pid: u32) -> bool {
        pid_probe(pid).is_ok()
    }

    /// Pins the probe contract the round-trip wait depends on: an UNREAPED
    /// zombie still owns its pid (the kernel answers `0`/`EPERM`, never
    /// `ESRCH`), and only the parent's reap turns the pid into `ESRCH`. A
    /// shell wrapper that merely exited non-zero can never satisfy this.
    #[cfg(unix)]
    #[test]
    fn pid_liveness_probe_requires_esrch_not_a_shell_verdict() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn the probe fixture");
        let pid = child.id();
        assert!(pid_alive(pid), "a running child owns its pid");
        // Observe the exit WITHOUT reaping it: `ps` state `Z` is a zombie.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let out = std::process::Command::new("/bin/ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .expect("ps the fixture");
            if String::from_utf8_lossy(&out.stdout)
                .trim_start()
                .starts_with('Z')
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the fixture child must exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            pid_alive(pid),
            "an unreaped zombie still exists: only ESRCH means gone"
        );
        child.wait().expect("reap the fixture");
        assert_eq!(
            pid_probe(pid),
            Err(libc::ESRCH),
            "after the parent reaps it, the kernel reports ESRCH"
        );
    }

    /// The production wiring end-to-end: an ACP client negotiates
    /// `faktor.terminal` and drives create → input → output → kill against
    /// the daemon's REAL session-owned authority (faktor-pty rows over the
    /// SAME session manager the backend uses). The terminal output must
    /// round-trip through a real PTY; the kill must take the real child.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acp_terminal_round_trip_runs_on_the_daemon_authority() {
        let rig = acp_shadow_rig(Vec::new());
        let authority_impl = Arc::new(DaemonTerminalAuthority::new(rig.session.clone()));
        let registry = authority_impl.registry.clone();
        let authority: Arc<dyn faktor_acp::TerminalAuthority> = authority_impl.clone();
        let (mut client, task) = start_daemon_terminal_server(rig.backend, Some(authority));

        let init = client
            .request(
                "initialize",
                json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
            )
            .await;
        assert!(
            init["result"]["extensions"]
                .as_array()
                .map(|names| names.iter().any(|n| n == "faktor.terminal"))
                .unwrap_or(false),
            "the attached authority must negotiate faktor.terminal: {init}"
        );

        let workspace = rig.dir.path().join("ws");
        let sid = acp_new_real_session(&mut client, &workspace).await;

        let created = client
            .request(
                "terminal/create",
                json!({
                    "sessionId": sid,
                    "command": "sh",
                    "args": ["-c", "stty -echo; read x; echo got:$x; sleep 30"],
                    "env": ["PATH"],
                    "rows": 24,
                    "cols": 80,
                }),
            )
            .await;
        assert_eq!(created["error"], Value::Null, "create failed: {created}");
        let tid = created["result"]["terminalId"]
            .as_str()
            .expect("terminalId")
            .to_string();
        let pid = created["result"]["pid"].as_u64().expect("pid") as u32;
        let ownership = created["result"]["ownershipId"]
            .as_str()
            .expect("ownershipId")
            .to_string();
        assert!(pid > 0, "a real pty pid");
        assert_eq!(created["result"]["sessionId"], json!(sid));

        // OWNERSHIP FIRST: the create-response contract is that the terminal
        // is already session-owned when the response arrives. Observe that
        // ownership condition with a bounded wait instead of assuming it; a
        // response that ever raced its registration fails HERE naming the
        // exact missing condition.
        let sid_typed = SessionId::new(sid.parse().unwrap());
        let ownership_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let rows = loop {
            let rows = registry.session_rows(sid_typed);
            if !rows.is_empty() {
                break rows;
            }
            assert!(
                std::time::Instant::now() < ownership_deadline,
                "terminal/create returned before its session-owned row was registered"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        // The row is session-owned in the daemon registry: the durable
        // session id, the operation id and the pid all match the wire.
        assert_eq!(rows.len(), 1, "one session-owned row: {rows:?}");
        assert_eq!(rows[0].0, tid);
        assert_eq!(rows[0].1.pid, pid);
        assert_eq!(rows[0].1.operation_id.to_string(), ownership);
        assert_eq!(rows[0].1.session_id.to_string(), sid);

        // LIVENESS of the OWNED row, with a bounded wait and never a single
        // instant sample: only ESRCH (the pid was reaped by the single
        // reader/reaper) proves the child is gone — 0 and EPERM both mean
        // the pid still exists (Darwin answers EPERM for unreaped zombies).
        let alive_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match pid_probe(pid) {
                Ok(()) => break,
                Err(libc::ESRCH) => {
                    let rows = registry.session_rows(sid_typed);
                    assert!(
                        std::time::Instant::now() < alive_deadline,
                        "the owned pty child (pid {pid}) was reaped right after create \
                         (session rows: {rows:?})"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(errno) => panic!(
                    "pid {pid} liveness probe failed with errno {errno}; the terminal \
                     cannot be observed"
                ),
            }
        }

        // Input → real tty → terminalOutput frames.
        let (response, updates) = client
            .request_until_output(
                "terminal/input",
                json!({ "sessionId": sid, "terminalId": tid, "data": "hello\n" }),
                &tid,
                "got:hello",
            )
            .await;
        assert_eq!(response["result"], json!({}), "{response}");
        assert!(
            !updates.is_empty(),
            "the real tty output must arrive as terminalOutput frames"
        );

        // Kill takes the real child; the bounded wait leaves ONLY when the
        // kernel reports ESRCH (the pid was reaped), never on a probe-side
        // verdict. EPERM keeps waiting: the pid still exists.
        let killed = client
            .request(
                "terminal/kill",
                json!({ "sessionId": sid, "terminalId": tid }),
            )
            .await;
        assert_eq!(killed["result"], json!({}), "{killed}");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match pid_probe(pid) {
                Err(libc::ESRCH) => break,
                Ok(()) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "terminal/kill must terminate the real child (pid {pid} still exists)"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(errno) => panic!(
                    "pid {pid} liveness probe failed with errno {errno}; the kill cannot be observed"
                ),
            }
        }

        drop(client);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }

    /// The negative contract on the wire: without a negotiated capability
    /// (or without an attached authority) every `terminal/*` method keeps
    /// the official `-32601` — never a partial success.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acp_terminal_methods_are_method_not_found_when_not_negotiated() {
        // (a) Authority attached, extension NOT negotiated.
        let rig = acp_shadow_rig(Vec::new());
        let authority: Arc<dyn faktor_acp::TerminalAuthority> =
            Arc::new(DaemonTerminalAuthority::new(rig.session.clone()));
        let (mut client, task) = start_daemon_terminal_server(rig.backend, Some(authority));
        client
            .request("initialize", json!({ "protocolVersion": 1 }))
            .await;
        let workspace = rig.dir.path().join("ws-no-neg");
        let sid = acp_new_real_session(&mut client, &workspace).await;
        let msg = client
            .request(
                "terminal/create",
                json!({ "sessionId": sid, "command": "sh", "args": ["-c", "true"] }),
            )
            .await;
        assert_eq!(
            msg["error"]["code"],
            json!(faktor_acp::METHOD_NOT_FOUND),
            "unnegotiated terminal/create must stay -32601: {msg}"
        );
        let msg = client
            .request("terminal/list", json!({ "sessionId": sid }))
            .await;
        assert_eq!(
            msg["error"]["code"],
            json!(faktor_acp::METHOD_NOT_FOUND),
            "unnegotiated terminal/list must stay -32601: {msg}"
        );
        drop(client);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

        // (b) Extension negotiated, but NO authority attached.
        let rig = acp_shadow_rig(Vec::new());
        let (mut client, task) = start_daemon_terminal_server(rig.backend, None);
        let init = client
            .request(
                "initialize",
                json!({ "protocolVersion": 1, "extensions": ["faktor.terminal"] }),
            )
            .await;
        assert!(
            init["result"].get("extensions").is_none()
                || !init["result"]["extensions"]
                    .as_array()
                    .map(|names| names.iter().any(|n| n == "faktor.terminal"))
                    .unwrap_or(false),
            "without an authority the extension is not negotiated: {init}"
        );
        let workspace = rig.dir.path().join("ws-no-authority");
        let sid = acp_new_real_session(&mut client, &workspace).await;
        let msg = client
            .request(
                "terminal/create",
                json!({ "sessionId": sid, "command": "sh", "args": ["-c", "true"] }),
            )
            .await;
        assert_eq!(
            msg["error"]["code"],
            json!(faktor_acp::METHOD_NOT_FOUND),
            "without an authority terminal/create must stay -32601: {msg}"
        );
        drop(client);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
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
                    install_root.join("artifacts").is_dir()
                        && install_root.join("staging").is_dir(),
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
                let plane =
                    faktor_worker::WorkerPlane::with_system_clock(std::sync::Arc::new(store));
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
    fn the_build_report_names_the_running_artifact_honestly() {
        // Directly started (no bootstrap env): the version is reported, the
        // self hash is optional and no release identity is claimed.
        let report = build_report_json(true);
        assert_eq!(report["schema"], "faktor-build-report/v1");
        assert_eq!(report["version"], faktor_core::VERSION);
        assert!(report["exe"].is_string());
        let self_sha = report["self_sha256"].as_str().unwrap();
        assert_eq!(self_sha.len(), 64);
        // No release environment in this test process (no other test sets
        // it): the release fields are honest absences, and the folded health
        // version keeps the plain package version.
        assert!(report["release_id"].is_null());
        assert!(report["release_digest"].is_null());
        assert!(report["self_digest_matches_release"].is_null());
        // The cheap daemon-startup form skips the full-binary hash pass.
        let cheap = build_report_json(false);
        assert!(cheap["self_sha256"].is_null());
        assert_eq!(cheap["version"], faktor_core::VERSION);
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
}
