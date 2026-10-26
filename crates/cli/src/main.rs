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

mod approval;
mod billing_cli;
mod billing_debits;
mod billing_report;
mod bootstrap;
mod config;
mod embeddings;
mod engineering;
mod evidence;
mod graph;
mod impact;
mod mcp_bridge;
mod mcp_oauth;
mod payload;
mod scm_daemon;
mod sso_auth;
mod supply;
#[cfg(test)]
mod test_http;
mod tools;
mod tools_market;
mod tools_supply_engineering;
mod worker_node;

use graph::DaemonGraph;

#[derive(Parser)]
#[command(
    name = "faktor",
    version,
    about = "Faktor — an engineering agent for code investigation, implementation and verification",
    long_about = "Faktor — an engineering agent for code investigation, implementation and verification.\n\n\
                  Daily:   run, doctor, sessions\n\
                  Service: serve, build, acp, bootstrap\n\
                  Admin:   commerce, enterprise, updater, worker"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Start the daemon; prints the Faktor startup line on stdout.
    #[command(display_order = 40)]
    Serve {
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        #[arg(long)]
        config: Option<String>,
    },
    /// Headless: create a session and run one prompt.
    #[command(display_order = 10)]
    Run {
        prompt: String,
        /// Provider instance id. Omitted, the daemon's single registered
        /// provider is used; zero or several registered providers refuse
        /// loudly instead of guessing (never a fake success).
        #[arg(long)]
        provider: Option<String>,
        #[arg(long, default_value = "default")]
        model: String,
        #[arg(long, default_value = ".")]
        workspace: String,
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
        #[arg(long)]
        config: Option<String>,
    },
    /// Self-check: storage, CAS, permissions, providers.
    #[command(display_order = 20)]
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
    #[command(display_order = 60)]
    Acp {
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Repository relationship graph authority: `graph` serializes the one
    /// document; `affected` returns impacted files/symbols/tests with
    /// reasons/provenance. CI and the semantic provider consume this same
    /// graph.
    #[command(display_order = 105)]
    Impact {
        #[command(subcommand)]
        action: impact::ImpactAction,
    },
    /// Signed updater lifecycle (local parity with the control-plane
    /// routes): status, check, stage, apply, rollback, recover and the
    /// explicitly authorized downgrade below the anti-rollback floor. The
    /// `[updater]` section must be enabled; manifests are verified against
    /// its operator key allowlist and downloads ride the checked transport.
    #[command(display_order = 100)]
    Updater {
        #[command(subcommand)]
        action: UpdaterAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Local commercial account plane (parity with the
    /// `/native/billing/*` and `/native/usage/export` routes): overview,
    /// bounded usage download and the owner-only subscription change/cancel
    /// effect. The `[billing]` section must be enabled.
    #[command(display_order = 95)]
    Billing {
        #[command(subcommand)]
        action: billing_cli::BillingAction,
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
    #[command(display_order = 90)]
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
    #[command(display_order = 110)]
    Worker {
        #[command(subcommand)]
        action: WorkerAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// The explicit sandbox shell choice (audit item 17): the fail-closed
    /// default or the operator's recorded network-capable shell grant.
    /// `status` reports the typed state; `choose` records the decision
    /// durably (config file + audit line), never silently.
    #[command(display_order = 25)]
    Sandbox {
        #[command(subcommand)]
        action: SandboxAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Manage remote MCP OAuth: login, status, logout.
    #[command(display_order = 45)]
    Mcp {
        #[command(subcommand)]
        action: mcp_oauth::McpAction,
    },
    /// List sessions.
    #[command(display_order = 30)]
    Sessions {
        #[arg(long, default_value = "~/.faktor")]
        data_dir: String,
    },
    /// Stable bootstrap launcher: resolve the activated release through the
    /// authenticated install pointer (`versions/<release-id>/faktor`), verify
    /// its signed manifest and digest, and exec exactly those bytes. Refuses
    /// (exit 3) on a missing pointer, an unsigned/tampered manifest or a
    /// digest mismatch; never falls back to another binary.
    #[command(display_order = 70)]
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
    #[command(display_order = 50)]
    Build,
    /// Faktor Acquire local admin (never a model tool): `doctor`, `status`,
    /// `login <source>`, `logout <source>`, `clear-cache`. Login opens the
    /// headed dedicated profile browser; credentials never enter the model
    /// context. The `[commerce]` section must be enabled for every action
    /// except reading the disabled `doctor`/`status` report.
    #[command(display_order = 80)]
    Commerce {
        #[command(subcommand)]
        action: CommerceAction,
        /// The daemon data dir (default `~/.faktor`).
        #[arg(long, default_value = "~/.faktor", global = true)]
        data_dir: String,
        #[arg(long, global = true)]
        config: Option<String>,
    },
    /// Deterministic Supply kernels over typed JSON: should-cost, TCO,
    /// supplier fit, qualification, customs, planning, RFQ, bid normalization
    /// and authorization-gated actions. No model, no network, no side effects.
    #[command(display_order = 85)]
    Supply {
        #[command(subcommand)]
        action: supply::SupplyAction,
    },
    /// Deterministic Engineering kernels over typed JSON: analyse a frozen
    /// requirement set against supply candidates, or verify a stored
    /// decision/receipt. No model, no network, no side effects.
    #[command(display_order = 86)]
    Engineering {
        #[command(subcommand)]
        action: engineering::EngineeringAction,
    },
    /// Human-operator approval authority for consequential Engineering and
    /// Supply actions: keygen, request, signed decide, list/status/audit.
    /// Agents can request; only a registered human key can decide.
    #[command(display_order = 87)]
    Approval {
        #[command(subcommand)]
        action: approval::ApprovalAction,
    },
}

/// The local sandbox admin subcommands (`faktor sandbox <action>`).
#[derive(Subcommand)]
pub(crate) enum SandboxAction {
    /// Print the typed first-run choice state: platform backend, durable
    /// record and the effective agent shell contract.
    Status,
    /// Record the explicit operator choice durably (decision document +
    /// append-only audit line). Exactly one of the two flags is required.
    Choose {
        /// Option (a): keep the fail-closed default (the secure default).
        #[arg(long)]
        fail_closed: bool,
        /// Option (b): grant network-capable shells; the exact trade-off is
        /// printed and recorded as an explicit operator grant.
        #[arg(long)]
        network_capable_shell: bool,
    },
}

/// The local Faktor Acquire subcommands (`faktor commerce <action>`).
#[derive(Subcommand)]
pub(crate) enum CommerceAction {
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
pub(crate) enum WorkerAction {
    /// Run the bounded worker loop: register once, then claim/execute/submit
    /// until the iteration budget or the stop condition. Prints one JSON
    /// tally line on stdout.
    #[command(display_order = 10)]
    Run {
        /// Loop budget (bounded by the worker crate; default 1 = one pass).
        #[arg(long)]
        iterations: Option<u32>,
    },
}

/// The enterprise local-parity subcommands (`faktor enterprise <action>`).
#[derive(Subcommand)]
pub(crate) enum EnterpriseAction {
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
pub(crate) enum EnterpriseArtifactAction {
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
pub(crate) enum EnterpriseDeletionAction {
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
pub(crate) enum UpdaterAction {
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

pub(crate) fn expand(p: &str) -> PathBuf {
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

/// The ONE standalone supervisor constructor for CLI-only commands
/// (doctor, impact). The daemon graph injects its own supervisor.
fn shared_supervisor() -> Result<std::sync::Arc<ProcessSupervisor>, faktor_core::error::Error> {
    ProcessSupervisor::try_shared()
}

#[tokio::main]
pub(crate) async fn main() {
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
            config,
        } => {
            run(
                prompt,
                provider.as_deref(),
                &model,
                expand(&workspace),
                expand(&data_dir),
                config.map(|c| expand(&c)),
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
        Command::Impact { action } => {
            let supervisor = match shared_supervisor() {
                Ok(supervisor) => supervisor,
                Err(error) => {
                    eprintln!("impact: process supervisor unavailable: {error}");
                    std::process::exit(1);
                }
            };
            if let Err(error) = impact::command(action, supervisor).await {
                eprintln!("impact: {error}");
                std::process::exit(1);
            }
        }
        Command::Updater {
            action,
            data_dir,
            config,
        } => {
            updater_command(action, expand(&data_dir), config.map(|c| expand(&c))).await;
        }
        Command::Billing {
            action,
            data_dir,
            config,
        } => {
            billing_cli::command(action, expand(&data_dir), config.map(|c| expand(&c))).await;
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
        Command::Supply { action } => {
            if let Err(error) = supply::command(action) {
                eprintln!("supply: {error}");
                std::process::exit(1);
            }
        }
        Command::Engineering { action } => {
            if let Err(error) = engineering::command(action) {
                eprintln!("engineering: {error}");
                std::process::exit(1);
            }
        }
        Command::Approval { action } => {
            if let Err(error) = approval::command(action) {
                eprintln!("approval: {error}");
                std::process::exit(1);
            }
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
        Command::Mcp { action } => {
            if let Err(error) = mcp_oauth::command(action).await {
                eprintln!("mcp: {error}");
                std::process::exit(1);
            }
        }
        Command::Sessions { data_dir } => {
            sessions(expand(&data_dir)).await;
        }
        Command::Sandbox {
            action,
            data_dir,
            config,
        } => {
            sandbox_command(action, expand(&data_dir), config.map(|c| expand(&c)));
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
pub(crate) fn build_report_json(include_self_digest: bool) -> Value {
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

/// Resolve the CLI's requested provider against the daemon's registry.
///
/// A requested id must be registered, and an omitted id resolves only when
/// exactly ONE provider is registered — zero or several refuse loudly. This
/// runs BEFORE any workspace/session effect: an unknown `--provider` must
/// never leave a failed session in the store (the old default
/// `--provider fake` did exactly that in every production build).
pub(crate) fn resolve_cli_provider(
    ids: &[String],
    requested: Option<&str>,
) -> Result<String, String> {
    if let Some(requested) = requested {
        if ids.iter().any(|id| id == requested) {
            return Ok(requested.to_string());
        }
        return Err(format!(
            "run: provider '{requested}' is not registered; registered: {ids:?}"
        ));
    }
    match ids.len() {
        1 => Ok(ids[0].clone()),
        0 => Err(
            "run: no providers are registered; configure providers in the daemon config or \
             pass --provider"
                .into(),
        ),
        _ => Err(format!(
            "run: multiple providers are registered ({ids:?}); pass --provider"
        )),
    }
}

/// Create the durable session one `faktor run` invocation drives and fire
/// the SessionStart lifecycle hook (audit) immediately after the row exists,
/// before the session is first used — the SAME ordering the ACP daemon entry
/// uses. Best-effort: the registry bounds the hook (deadline/caps) and a
/// failing verdict is audit-only, so session creation can never fail or hang
/// unboundedly on a hook. A registry-less runtime is a no-op.
pub(crate) fn create_run_session(
    session: &Arc<SessionManager>,
    agent: &Arc<AgentRuntime>,
    ws: faktor_core::id::WorkspaceId,
    provider: &str,
    model: &str,
) -> Result<faktor_session::SessionHandle, String> {
    let row = session
        .create_session(ws, "cli run", provider, model)
        .map_err(|e| e.to_string())?;
    agent.run_session_start_hook(row.id());
    Ok(row)
}

/// The assistant's durable text output for the newest completed turn, in
/// part order. Only `text` parts are assistant output: reasoning parts are
/// the model's private thinking and tool parts are activity, never the
/// answer. Used by the headless `run` command (the result must be printed,
/// not only its state).
pub(crate) fn assistant_text(
    handle: &faktor_session::SessionHandle,
) -> faktor_core::Result<String> {
    let rows = handle.messages_before(None, 16)?;
    for row in rows {
        if row.role != "assistant" {
            continue;
        }
        let mut out = String::new();
        for part in handle.parts_of(row.id)? {
            if part.kind == "text" {
                if let Some(text) = part.data.get("text").and_then(|v| v.as_str()) {
                    out.push_str(text);
                }
            }
        }
        return Ok(out);
    }
    Ok(String::new())
}

pub(crate) async fn run(
    prompt: String,
    provider: Option<&str>,
    model: &str,
    workspace: PathBuf,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    // `run` used to build an EMPTY registry (no config discovery, no flag),
    // so it could never see a provider and its refusal text named a surface
    // it did not have. Explicit --config is strict; the discovered
    // `<data-dir>/faktor-plus.json` is lenient, exactly like serve/acp.
    let config = match load_optional_config(&data_dir, config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("run: config error: {e}");
            std::process::exit(1);
        }
    };
    // Present the typed first-run sandbox choice (audit item 17) before the
    // daemon graph exists: fail-closed stays the effective default unless
    // the operator recorded the grant explicitly.
    config::announce_startup(&data_dir, &config);
    // PURE preflight BEFORE the daemon build touches the data dir: a pinned/
    // balanced routing refusal, an embeddings-policy refusal or a provider
    // resolution refusal (`--provider ghost`) must leave NO store/db/CAS
    // behind. The registered ids come from the SAME shared provider builder
    // the daemon core uses.
    let preflight = match preflight_daemon_config(&config) {
        Ok(preflight) => preflight,
        Err(e) => {
            eprintln!("run: config error: {e}");
            std::process::exit(1);
        }
    };
    let provider = match resolve_cli_provider(&preflight.provider_ids, provider) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    match build_daemon_with_permission(
        &data_dir,
        Some(config),
        std::sync::Arc::new(HeadlessPermissionRequester),
    ) {
        Ok(graph) => {
            let (session, agent) = (graph.session, graph.agent);
            let ws = match session.create_workspace(workspace.to_str().unwrap_or(".")) {
                Ok(ws) => ws,
                Err(e) => {
                    eprintln!("workspace error: {e}");
                    std::process::exit(1);
                }
            };
            let row = match create_run_session(&session, &agent, ws, &provider, model) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("session error: {e}");
                    std::process::exit(1);
                }
            };
            match agent.run_turn(row.id(), &prompt, &[]).await {
                Ok(outcome) => {
                    let label = outcome.final_state.label();
                    if !matches!(
                        outcome.final_state,
                        faktor_core::state::AgentState::ReadyForNextTurn
                    ) {
                        // Headless callers must be able to detect failure
                        // without parsing text: a turn that did not reach a
                        // continuation-ready state is a non-zero exit.
                        eprintln!("run: turn ended in state '{label}'");
                        std::process::exit(1);
                    }
                    // Headless contract: the assistant's durable text is the
                    // command's output. The turn used to exit 0 after printing
                    // only its state, so a caller saw no model answer at all.
                    match assistant_text(&row) {
                        Ok(text) if !text.is_empty() => println!("{text}"),
                        Ok(_) => eprintln!("run: the turn completed with no assistant text"),
                        Err(e) => eprintln!("run: cannot read assistant output: {e}"),
                    }
                    println!("final state: {label}");
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

/// `faktor sandbox <action>` — the typed first-run choice surface (audit
/// item 17). `status` is read-only; `choose` records the operator's
/// explicit decision durably and prints the exact trade-off. The command
/// never changes behavior implicitly: choosing (a) keeps the secure
/// fail-closed default, choosing (b) records the strictly weaker grant.
pub(crate) fn sandbox_command(
    action: SandboxAction,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    let config = match load_optional_config(&data_dir, config_path) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("faktor sandbox: config error: {e}");
            std::process::exit(1);
        }
    };
    match action {
        SandboxAction::Status => {
            let state = config::inspect(&data_dir, &config);
            println!(
                "sandbox first-run choice: decision={} platform={} backend_available={}",
                state.decision.as_tag(),
                std::env::consts::OS,
                state.backend_available,
            );
            println!(
                "decision document: {}",
                config::decision_path(&data_dir).display()
            );
            println!("audit trail: {}", config::audit_path(&data_dir).display());
            if let Some(reason) = &state.corrupt_reason {
                // An unconfirmed or unreadable document is NEVER printed as
                // a recorded decision: the state above already resolves
                // fail-closed, and the loud reason tells the operator what
                // to fix.
                eprintln!("faktor sandbox: decision unreadable: {reason}");
                std::process::exit(1);
            }
            if let Some(record) = &state.record {
                println!(
                    "recorded decision: choice={} at_ms={} platform={} backend_available={}",
                    record.choice.as_tag(),
                    record.decided_at_ms,
                    record.platform,
                    record.network_backend_available,
                );
                if !record.trade_off.is_empty() {
                    println!("recorded trade-off: {}", record.trade_off);
                }
            }
            if let Some(text) = config::startup_notice(&state, &data_dir) {
                println!("{text}");
            }
        }
        SandboxAction::Choose {
            fail_closed,
            network_capable_shell,
        } => {
            if fail_closed == network_capable_shell {
                eprintln!(
                    "faktor sandbox choose: select exactly one of --fail-closed or \
                     --network-capable-shell"
                );
                std::process::exit(2);
            }
            if config.sandbox.shell.is_some() {
                eprintln!(
                    "faktor sandbox choose: the config file explicitly sets [sandbox] shell; \
                     that explicit key always wins — edit the config file instead of recording \
                     a durable choice"
                );
                std::process::exit(1);
            }
            let choice = if fail_closed {
                config::SandboxShellChoice::FailClosed
            } else {
                config::SandboxShellChoice::NetworkCapableUserGranted
            };
            match config::record_decision(&data_dir, choice, now_ms()) {
                Ok(record) => {
                    println!(
                        "sandbox first-run choice recorded: {} (platform={}, \
                         backend_available={})",
                        record.choice.as_tag(),
                        record.platform,
                        record.network_backend_available,
                    );
                    println!(
                        "recorded at: {}",
                        config::decision_path(&data_dir).display()
                    );
                    println!("audit trail: {}", config::audit_path(&data_dir).display());
                    if choice == config::SandboxShellChoice::NetworkCapableUserGranted {
                        println!("trade-off: {}", config::NETWORK_CAPABLE_SHELL_TRADE_OFF);
                    } else {
                        println!(
                            "the secure fail-closed default stays in force: agent shell and \
                             verification children remain OS-isolation-required"
                        );
                    }
                }
                Err(e) => {
                    eprintln!("faktor sandbox choose: cannot record the decision: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// Human-readable outcome of one doctor run. The shell wrapper prints the
/// lines and exits non-zero when `issues > 0`; tests call [`doctor_run`]
/// directly so a failing run never exits the test process.
pub(crate) struct DoctorReport {
    pub(crate) lines: Vec<String>,
    pub(crate) issues: usize,
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
pub(crate) struct CliDoctorProbe {
    pub(crate) data_dir: PathBuf,
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
pub(crate) fn build_local_updater(
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
pub(crate) fn read_manifest_file(path: &std::path::Path) -> Result<Vec<u8>, String> {
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

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `faktor updater <action>` — local parity with the control-plane routes.
pub(crate) async fn updater_command(
    action: UpdaterAction,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    let (config, _semantic) = match load_entry_config(&data_dir, config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("faktor updater: config error: {e}");
            std::process::exit(1);
        }
    };
    let probe: Arc<dyn faktor_updater::HealthProbe> =
        Arc::new(faktor_updater::DataStoreSchemaProbe::new(
            CliDoctorProbe {
                data_dir: data_dir.clone(),
            },
            data_dir.join("store").join("faktor-plus.db"),
            faktor_store::SCHEMA_HEAD,
        ));
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
pub(crate) fn load_optional_config(
    data_dir: &std::path::Path,
    config_path: Option<PathBuf>,
) -> Result<config::Config, String> {
    Ok(load_entry_config(data_dir, config_path)?.0)
}

/// ONE config resolution for every entry point (serve/acp/run/updater/
/// enterprise/commerce/worker): an explicit `--config` is STRICT (parse +
/// validate + unknown fields refused); without one, `<data-dir>/
/// faktor-plus.json` is DISCOVERED leniently (a broken file warns and falls
/// back to defaults). The additive top-level `semantic` section is stripped
/// from the raw document before the frozen `Config` shape sees it, so no
/// entry point can reject a serve-valid file or silently discard the rest
/// of it.
pub(crate) fn load_entry_config(
    data_dir: &std::path::Path,
    config_path: Option<PathBuf>,
) -> Result<(config::Config, graph::SemanticCfg), String> {
    let explicit = config_path.is_some();
    let path = match config_path {
        Some(path) => path,
        None => {
            let discovered = data_dir.join("faktor-plus.json");
            if !discovered.exists() {
                let mut config = config::Config::default();
                config::apply_effective_choice(&mut config, data_dir);
                return Ok((config, graph::SemanticCfg::default()));
            }
            discovered
        }
    };
    match load_config_document(&path) {
        Ok((mut config, semantic)) => {
            // Apply the durable first-run sandbox choice (audit item 17): an
            // explicit `[sandbox] shell` key always wins; otherwise a
            // recorded explicit grant switches the agent shell class to the
            // app-level-only network-capable mode. Never widens silently —
            // the grant is only ever applied from the operator's recorded
            // decision.
            config::apply_effective_choice(&mut config, data_dir);
            Ok((config, semantic))
        }
        Err(e) if explicit => Err(e),
        Err(e) => {
            tracing::error!("config error: {e}; using defaults");
            let mut config = config::Config::default();
            config::apply_effective_choice(&mut config, data_dir);
            Ok((config, graph::SemanticCfg::default()))
        }
    }
}

/// Parse one config document: extract `[semantic]`, then parse + validate
/// the frozen `Config` shape.
pub(crate) fn load_config_document(
    path: &std::path::Path,
) -> Result<(config::Config, graph::SemanticCfg), String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("config {}: {e}", path.display()))?;
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
    Ok((config, semantic))
}

/// The local Faktor Acquire admin entry (`faktor commerce <action>`). Never
/// a model tool: no command here is registered in the agent's ToolRegistry,
/// and `login` hands the interactive flow to the headed profile browser.
pub(crate) async fn commerce_command(
    action: CommerceAction,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    let config = match load_optional_config(&data_dir, config_path) {
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

pub(crate) async fn enterprise_command(
    action: EnterpriseAction,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) {
    let (config, _semantic) = match load_entry_config(&data_dir, config_path) {
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
pub(crate) async fn doctor(data_dir: PathBuf, deep: bool, config_path: Option<PathBuf>) {
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
pub(crate) fn doctor_run(data_dir: &std::path::Path, deep: bool) -> DoctorReport {
    doctor_run_with_config(data_dir, deep, None)
}

/// `doctor [--config <path>]`: the storage checks plus, when an explicit
/// config is named, the `[worker_plane]` deployment-boundary audit. The
/// boundary decision (including the `trusted_gateway` acknowledgement) is
/// surfaced line-by-line; a refused boundary is an ISSUE (the daemon would
/// refuse to start) and is never repaired.
pub(crate) fn doctor_run_with_config(
    data_dir: &std::path::Path,
    deep: bool,
    config_path: Option<&std::path::Path>,
) -> DoctorReport {
    let mut lines: Vec<String> = Vec::new();
    let mut issues = 0usize;
    doctor_storage_governor_line(data_dir, &mut lines, &mut issues);
    doctor_worker_plane_line(config_path, &mut lines, &mut issues);
    // The additive value-centric entitlement section: the [billing] plan
    // table's paid VALUE features (inference-independent; offline default
    // denies typed while the local product is unaffected).
    doctor_value_entitlements_line(config_path, &mut lines, &mut issues);
    doctor_sandbox_shell_line(config_path, data_dir, &mut lines, &mut issues);
    doctor_sandbox_first_run_line(data_dir, config_path, &mut lines, &mut issues);
    doctor_sandbox_agent_home_line(data_dir, config_path, &mut lines, &mut issues);
    doctor_network_isolation_line(&mut lines);
    doctor_tokenizer_certification_line(config_path, &mut lines, &mut issues);
    doctor_capability_authority_line(config_path, &mut lines, &mut issues);
    doctor_contract_compiler_line(&mut lines, &mut issues);
    doctor_live_canary_line(&mut lines);
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
            // The additive engineering-outcome ledger section (economic
            // audit item 7): the durable completed-run metrics aggregate,
            // read-only. A ledger read failure is an ISSUE, never silence.
            doctor_outcome_ledger_line(store.clone(), &mut lines, &mut issues);
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

/// The always-on storage governor section: totals, every per-class quota and
/// usage, evictions, refusals, pending reservations and the watermark.
/// Degraded health is an ISSUE (the budget cannot be met). Doctor never
/// reconciles or evicts: it reports the durable ledger as-is.
pub(crate) fn doctor_storage_governor_line(
    data_dir: &std::path::Path,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(governor) = crate::config::StorageCfg::existing_governor(data_dir) else {
        lines.push("storage-governor: not initialized (next daemon start creates it)".into());
        return;
    };
    match governor.metrics() {
        Ok(metrics) => {
            let classes = metrics
                .per_class
                .values()
                .map(|usage| {
                    format!(
                        "{}={}/{}",
                        usage.class.as_str(),
                        usage.bytes,
                        usage.quota_bytes
                    )
                })
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!(
                "storage-governor: total={}/{} bytes ({} permille) objects={} evictions={} refusals={} pending={} loss-markers={} watermark={} permille above-watermark={} last-maintenance-ms={} classes=[{}]",
                metrics.total_bytes,
                metrics.total_budget_bytes,
                metrics.used_permille_of_budget,
                metrics.total_objects,
                metrics.evictions,
                metrics.refusals,
                metrics.pending_reservations,
                metrics.loss_markers,
                metrics.watermark_permille,
                metrics.above_watermark,
                metrics
                    .last_maintenance_ms
                    .map(|ms| ms.to_string())
                    .unwrap_or_else(|| "never".to_string()),
                classes
            ));
            if let Some(degraded) = metrics.degraded {
                lines.push(format!(
                    "storage-governor: DEGRADED {} — {} (total={} budget={} required={} class={})",
                    degraded.code,
                    degraded.detail,
                    degraded.total_bytes,
                    degraded.total_budget_bytes,
                    degraded.required_bytes,
                    degraded
                        .class
                        .map(|class| class.as_str())
                        .unwrap_or("aggregate")
                ));
                *issues += 1;
            }
        }
        Err(error) => {
            lines.push(format!("storage-governor: metrics FAILED ({error})"));
            *issues += 1;
        }
    }
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
pub(crate) fn commercial_db_doctor(
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
    const SKIP_DB_DIRS: &[&str] = &["backups", "commercial-backups", "cas", "storage-governor"];
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
pub(crate) fn discover_commercial_dbs(
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
pub(crate) const MAX_DOCTOR_INDEX_WORKSPACES: usize = 32;

/// Bound on one published generation file read for the embedding build
/// status. A doctor probe is an interactive check, not a load path: a larger
/// index is NAMED as unprobed (the daemon itself still serves it).
pub(crate) const MAX_DOCTOR_INDEX_GENERATION_BYTES: u64 = 64 * 1024 * 1024;

/// Bound of the persisted degraded reason rendered on one doctor line: a
/// hostile generation cannot inject newlines or balloon the report.
pub(crate) const MAX_DOCTOR_INDEX_REASON_CHARS: usize = 160;

/// The minimal slice of a published generation file the doctor decodes: the
/// envelope identity plus the persisted embedding build record. Every other
/// (potentially huge) member is skipped by serde without materialization, so
/// the probe's memory stays bounded no matter how large the published index
/// is.
#[derive(serde::Deserialize)]
pub(crate) struct DoctorGenerationStatus {
    pub(crate) format: u32,
    pub(crate) workspace: u64,
    pub(crate) generation: u64,
    pub(crate) data: DoctorGenerationData,
    /// Durable index coverage (audits 5/6): absent on pre-coverage
    /// envelopes, which decode as legacy-unknown (INCOMPLETE, never
    /// complete) and are reported as such below.
    #[serde(default)]
    pub(crate) coverage: Option<faktor_index::IndexCoverage>,
    /// Durable fingerprint shard coverage (audit 6).
    #[serde(default)]
    pub(crate) fingerprint_coverage: Option<faktor_index::FingerprintCoverage>,
}

/// The decoded published-generation facts the doctor reports: the typed
/// embedding build status plus the durable coverage records (additive).
pub(crate) struct DoctorPublishedStatus {
    pub(crate) embedding: faktor_index::embedding::EmbeddingBuildStatus,
    pub(crate) coverage: Option<faktor_index::IndexCoverage>,
    pub(crate) fingerprint_coverage: Option<faktor_index::FingerprintCoverage>,
}

#[derive(serde::Deserialize)]
pub(crate) struct DoctorGenerationData {
    #[serde(default)]
    pub(crate) embeddings: DoctorGenerationEmbeddings,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct DoctorGenerationEmbeddings {
    #[serde(default)]
    pub(crate) format: u32,
    #[serde(default)]
    pub(crate) build: faktor_index::embedding::EmbeddingBuildRecord,
}

/// The Engineering Outcome Ledger doctor section (economic audit item 7):
/// one bounded read-only line over the durable completed-run ledger
/// aggregates (`faktor-agent`'s `OutcomeLedger` metrics fold; the same fold
/// `GET /native/outcomes` serves). An empty ledger is a truthful zero line;
/// a read/parse failure is an ISSUE — doctor never repairs and never
/// fabricates a number.
pub(crate) fn doctor_outcome_ledger_line(
    store: Arc<faktor_store::Store>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let ledger = faktor_agent::OutcomeLedger::new(store);
    match ledger.metrics() {
        Ok(report) => {
            let m = &report.metrics;
            if m.runs == 0 {
                lines.push("outcomes: no completed runs recorded".into());
                return;
            }
            // Economic-truth P1: monetary figures describe the
            // money-complete cohort ONLY. Each figure is gated on its OWN
            // served denominator (the same rule the IDE panels apply): the
            // cost figures on `verifiedChangesMoneyComplete`, the rework
            // ratio on the money-complete cohort's total spend, the
            // first-verification-pass rate on verified changes — an empty
            // denominator says `unavailable`, never a fabricated zero, and
            // the excluded rows are always counted.
            let money_text = |ready: bool, value: u64| -> String {
                if ready {
                    format!("{value} microUSD")
                } else {
                    "unavailable".into()
                }
            };
            let verified_money_complete = m.verified_changes_money_complete > 0;
            let success_cost = money_text(verified_money_complete, m.successful_run_cost_micro);
            let all_in_cost = money_text(
                verified_money_complete,
                m.verified_cost_per_successful_change_micro,
            );
            // Rework = rework spend ÷ (model + verification) spend; a zero
            // money-complete spend is an empty denominator (unmeasured),
            // exactly like the panels' `totalSpendMicro` gate.
            let rework = if m.total_spend_micro > 0 {
                format!("{}%", doctor_ppm_percent(m.rework_cost_ratio_ppm))
            } else {
                "unavailable".into()
            };
            let first_pass = if m.verified_changes > 0 {
                format!(
                    "{}%",
                    doctor_ppm_percent(m.verified_first_verification_pass_ppm)
                )
            } else {
                "unavailable".into()
            };
            let exclusion = if m.money_incomplete_excluded > 0 {
                format!(
                    ", {} money-incomplete excluded (money metrics over {} complete run(s))",
                    m.money_incomplete_excluded, m.money_complete_runs
                )
            } else {
                String::new()
            };
            // Local classification coverage (mirrors the frontier rule): the
            // boundary-class state plus the classed cohort, never a silent
            // full-coverage claim over a folded/classless history.
            let local_coverage = match m.local_measurement {
                faktor_agent::LocalMeasurement::Measured => "measured".to_string(),
                faktor_agent::LocalMeasurement::Partial => {
                    format!(
                        "partial ({}/{} classed)",
                        m.local_classed_runs, m.runs_with_steps
                    )
                }
                faktor_agent::LocalMeasurement::Unmeasured => "unmeasured".to_string(),
            };
            // Attribution honesty: a non-complete run never fed the ledger's
            // routing-learning feed (its samples kept the per-call feed);
            // the exclusion is always counted when nonzero.
            let attribution = if m.attribution_incomplete_excluded > 0 {
                format!(
                    ", {} attribution-incomplete excluded from learning (partial {}, unavailable {})",
                    m.attribution_incomplete_excluded,
                    m.attribution_partial_runs,
                    m.attribution_unavailable_runs
                )
            } else {
                String::new()
            };
            lines.push(format!(
                "outcomes: {} run(s), {} verified, first-verification-pass {}, successful-run cost {}, \
                 all-in cost/verified change {}, rework ratio {}, rollback {}%, local-class coverage {local_coverage}{attribution}{exclusion}{truncated}",
                m.runs,
                m.verified_changes,
                first_pass,
                success_cost,
                all_in_cost,
                rework,
                doctor_ppm_percent(m.regression_rollback_ppm),
                truncated = if report.truncated {
                    format!(" (metrics folded the newest {} runs)", report.scanned)
                } else {
                    String::new()
                },
            ));
        }
        Err(e) => {
            lines.push(format!("outcomes: FAILED ({e})"));
            *issues += 1;
        }
    }
}

/// One ppm rate as a one-decimal percent string (`750000 -> "75.0"`).
pub(crate) fn doctor_ppm_percent(ppm: u32) -> String {
    let whole = ppm / 10_000;
    let tenth = (ppm % 10_000) / 1_000;
    format!("{whole}.{tenth}")
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
pub(crate) fn index_embedding_doctor(
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
            Ok(Some(published)) => {
                lines.push(format_index_embedding_line(
                    raw,
                    generation,
                    &published.embedding,
                ));
                lines.push(format_index_coverage_line(
                    raw,
                    generation,
                    published.coverage.as_ref(),
                    published.fingerprint_coverage.as_ref(),
                ));
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
pub(crate) fn doctor_index_state_label(state: &faktor_index::WorkspaceIndexState) -> &'static str {
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
pub(crate) fn read_published_embedding_status(
    path: &std::path::Path,
    raw: u64,
    generation: u64,
) -> Result<Option<DoctorPublishedStatus>, String> {
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
    let embedding = if decoded.data.embeddings.format != faktor_index::embedding::EMBEDDING_FORMAT {
        faktor_index::embedding::EmbeddingBuildStatus::Unconfigured
    } else {
        decoded.data.embeddings.build.status()
    };
    Ok(Some(DoctorPublishedStatus {
        embedding,
        coverage: decoded.coverage,
        fingerprint_coverage: decoded.fingerprint_coverage,
    }))
}

/// One additive doctor line per workspace: the durable index coverage
/// (audits 5/6). A pre-coverage envelope without a coverage record is NAMED
/// as legacy-unknown INCOMPLETE (its completeness is UNKNOWN and a resumable
/// rebuild is pending), never silently reported as covered; an incomplete
/// generation names its batch reason and counters.
pub(crate) fn format_index_coverage_line(
    raw: u64,
    generation: u64,
    coverage: Option<&faktor_index::IndexCoverage>,
    fingerprint: Option<&faktor_index::FingerprintCoverage>,
) -> String {
    let coverage_text = match coverage {
        None => "coverage=legacy_unknown (no record; INCOMPLETE, rebuild pending)".to_string(),
        Some(c) if c.complete => format!(
            "coverage=complete files_seen={} files_indexed={} bytes_indexed={}",
            c.files_seen, c.files_indexed, c.bytes_indexed
        ),
        Some(c) => format!(
            "coverage=INCOMPLETE reason={} files_seen={} files_indexed={} bytes_indexed={}",
            c.truncated_reason.as_deref().unwrap_or("unspecified"),
            c.files_seen,
            c.files_indexed,
            c.bytes_indexed
        ),
    };
    let fingerprint_text = match fingerprint {
        None => "fingerprint=legacy_unknown (no record; PARTIAL, not a clean baseline)".to_string(),
        Some(f) if f.complete => "fingerprint=complete".to_string(),
        Some(f) => format!(
            "fingerprint=PARTIAL shard={}/{} round_start={} reason={}",
            f.shard,
            faktor_index::FINGERPRINT_SHARDS,
            f.round_start,
            f.truncated_reason.as_deref().unwrap_or("unspecified")
        ),
    };
    format!(
        "index coverage: workspace {raw}: generation {generation}: {coverage_text}; {fingerprint_text}"
    )
}

/// One line per workspace: the typed state name is exact (`unconfigured`,
/// `complete`, `bounded`, `degraded`); a degraded build additionally names
/// the affected chunk count and the bounded provider reason.
pub(crate) fn format_index_embedding_line(
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
pub(crate) fn doctor_index_reason_fragment(reason: &str) -> String {
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

/// The doctor's shell-execution surface (item 10): reports the effective
/// shell contract of BOTH trust classes with their honest strength labels,
/// so an OS-isolated shell and a user-granted network-capable shell are
/// never presented as equivalent and an interactive-terminal grant is never
/// mistaken for the agent shell-tool contract:
///
/// - `agent shell tool`: `Config::sandbox_policy` — the policy behind every
///   agent-generated `run_command`/supervised shell spawn (secure
///   `os_isolated`/`required` default when `[sandbox] shell` is unset).
/// - `interactive session terminals`: [`terminal_authority_policy`] — the
///   policy injected into the daemon terminal authority (the honest
///   user-granted default when `[sandbox] shell` is unset; isolation only
///   when the operator configures it explicitly).
///
/// Uses the explicit `--config` when given and the daemon defaults
/// otherwise; a config the daemon would refuse is reported as an issue,
/// never silently rendered as a healthy state.
pub(crate) fn doctor_sandbox_shell_line(
    config_path: Option<&std::path::Path>,
    data_dir: &std::path::Path,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(mut cfg) = doctor_sandbox_config(config_path) else {
        return;
    };
    // Resolve the EFFECTIVE contract exactly as every daemon entry point
    // does (config key first, then the durable first-run record): doctor
    // must print the mode the daemon would enforce, never a config-only
    // shadow of it.
    config::apply_effective_choice(&mut cfg, data_dir);
    push_shell_class_line(
        "agent shell tool",
        cfg.sandbox_policy()
            .map(|policy| policy.shell_execution_state()),
        lines,
        issues,
    );
    push_shell_class_line(
        "interactive session terminals",
        terminal_authority_policy(&cfg).map(|authority| authority.shell_execution_state()),
        lines,
        issues,
    );
}

/// One doctor line for one shell-execution trust class. The state is read
/// back from the SAME construction seam the daemon injects, so doctor prints
/// exactly the mode that class enforces and records; a refusal is an issue,
/// never a silently rendered healthy state.
pub(crate) fn push_shell_class_line(
    class: &str,
    state: Result<faktor_sandbox::ShellExecutionState, String>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    match state {
        Ok(state) => lines.push(format!(
            "sandbox shell execution ({class}): {} (mode={}, network_guarantee={})",
            state.strength_label(),
            state.mode.as_tag(),
            state.network_guarantee.as_tag(),
        )),
        Err(e) => {
            lines.push(format!("sandbox shell execution ({class}): FAILED {e}"));
            *issues += 1;
        }
    }
}

/// The doctor's first-run sandbox-choice surface (audit item 17): the typed
/// decision the daemon would actually run under — explicit config key,
/// recorded operator grant, recorded fail-closed, or PENDING on a
/// backend-less platform. Pending is NOT an issue (the secure default is
/// correct); an unreadable decision document IS an issue and the state
/// remains fail-closed.
pub(crate) fn doctor_sandbox_first_run_line(
    data_dir: &std::path::Path,
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(config) = doctor_sandbox_config(config_path) else {
        return;
    };
    let state = config::inspect(data_dir, &config);
    let record = state
        .record
        .as_ref()
        .map(|record| record.choice.as_tag())
        .unwrap_or("none");
    lines.push(format!(
        "sandbox first-run choice: decision={} platform={} network_backend={} record={}",
        state.decision.as_tag(),
        std::env::consts::OS,
        if state.backend_available {
            "available"
        } else {
            "absent"
        },
        record,
    ));
    if let Some(text) = config::startup_notice(&state, data_dir) {
        lines.push(text);
    }
    if let Some(reason) = &state.corrupt_reason {
        lines.push(format!("sandbox first-run decision unreadable: {reason}"));
        *issues += 1;
    }
}

/// The doctor's agent-HOME surface (audit item 6): the effective HOME
/// contract and the honest credential-confinement state of this platform.
pub(crate) fn doctor_sandbox_agent_home_line(
    data_dir: &std::path::Path,
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    _issues: &mut usize,
) {
    let Some(config) = doctor_sandbox_config(config_path) else {
        return;
    };
    let policy = config.sandbox.home;
    lines.push(format!(
        "sandbox agent home: policy={} scratch_root={}",
        policy.as_tag(),
        data_dir.join(faktor_terminal::AGENT_HOME_DIR).display(),
    ));
    if policy.passes_real_home() {
        lines.push(
            "sandbox agent home grant: EXPLICIT operator grant passes the real HOME through; \
             credential directories stay denied by OS-level confinement wherever a backend is \
             installed"
                .to_string(),
        );
    }
    let credential_state = if faktor_sandbox::filesystem_backend_available() {
        "kernel-denied by the confinement policy (the OS backend never grants ~/.ssh, ~/.aws, \
         ~/.config/gcloud, ~/.netrc or ~/.git-credentials; a Required confinement root that \
         would cover one is a typed spawn refusal); on this platform the backend exists"
            .to_string()
    } else {
        format!(
            "honest limitation: no OS confinement backend on {} — the per-session scratch HOME \
             keeps $HOME-relative credential discovery empty, but absolute-path reads are not \
             OS-denied",
            std::env::consts::OS
        )
    };
    lines.push(format!("sandbox credential dirs: {credential_state}"));
}

/// The one config resolution for the new sandbox doctor lines: the explicit
/// `--config` document (already validated by `doctor_worker_plane_line`,
/// which reports a refusal loudly) or the daemon defaults. `None` means a
/// refused config was already surfaced and nothing may be implied from it.
fn doctor_sandbox_config(config_path: Option<&std::path::Path>) -> Option<config::Config> {
    match config_path {
        Some(path) => serve_config_and_semantic(Some(path.to_path_buf()))
            .ok()
            .map(|(config, _semantic)| config),
        None => Some(config::Config::default()),
    }
}

/// The honest network-isolation strength of this build (audit item 8): the
/// per-process spawn backend this platform has, what a spawn has PROVEN at
/// runtime in this process, and the exact isolation every browser launch
/// selects. Purely informational and never config-gated: no backend on this
/// platform is a documented platform fact (macOS/Windows are a typed
/// platform boundary — every DenyAll/BrokerOnly spawn is refused typed
/// before any child exists), never a doctor issue. The wording names the
/// platform and never presents app-level proxy configuration as confinement.
pub(crate) fn doctor_network_isolation_line(lines: &mut Vec<String>) {
    let backend = if faktor_terminal::broker_only_supported() {
        "deny_all+broker_only_available".to_string()
    } else if faktor_terminal::deny_all_supported() {
        format!(
            "deny_all_available (BrokerOnly unavailable on {}: browser launches stay \
             proxy-only at the application level and BrokerOnly requests are refused typed)",
            std::env::consts::OS
        )
    } else {
        format!(
            "unavailable (typed platform boundary: no per-process network-isolation backend \
             on {}; DenyAll and BrokerOnly spawns are refused typed, proxy flags are \
             application configuration only)",
            std::env::consts::OS
        )
    };
    let proof = faktor_terminal::platform_network_enforcement();
    lines.push(format!(
        "sandbox network isolation: backend={backend} runtime_proof={}{}",
        proof.as_tag(),
        if proof == faktor_terminal::NetworkEnforcement::AppLevel {
            " (no isolated spawn has proven itself in this process)"
        } else {
            ""
        },
    ));
    lines.push(format!(
        "browser network isolation: {}",
        faktor_browser::platform_isolation_label()
    ));
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
/// "enabled" — so the doctor cannot certify a daemon that would not start.
pub(crate) fn doctor_worker_plane_line(
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

/// The additive live-canary status section (marketplace audit item 8): one
/// informational line per marketplace source, `configured` when every
/// opt-in canary environment variable is present, else `absent`. This is
/// always information, never an ISSUE: absence is the default, and no
/// vendor request happens without an operator explicitly providing the
/// credentials (live-canary registry in `faktor-commerce-connectors`; legal
/// posture in `docs/legal/marketplace-acquisition.md`).
pub(crate) fn doctor_live_canary_line(lines: &mut Vec<String>) {
    for (source, status) in faktor_commerce_connectors::canary::canary_statuses_from_env() {
        lines.push(format!("live-canary {source}: {}", status.label()));
    }
}

/// The value-centric entitlement doctor section (additive; product closure):
/// reports the `[billing]` plan table's VALUE entitlements — the paid
/// product capabilities that are independent of inference volume (advanced
/// verification policy, organization contracts, shared/cross-repo
/// intelligence, outcome analytics, signed proof/provenance, audit
/// retention history, model-optimization learning, private runners,
/// enterprise governance with its SSO/SCIM capability flags).
///
/// Reporting rules:
///
/// - no `[billing]` section (or disabled): the OFFLINE DEFAULT is reported
///   honestly — value features are denied typed and the local product is
///   unaffected; this is never an ISSUE (local-only deployments stay fully
///   functional);
/// - enabled: one line per plan maps the plan's value feature tags and
///   effective governance flags. A plan mapping nothing is legal (a pure
///   inference-billing plan) and informational;
/// - a config with a billing-specific refusal is an ISSUE; a config the
///   daemon refuses wholesale is reported as refused WITHOUT a second issue
///   (the worker-plane line above already counts it once).
pub(crate) fn doctor_value_entitlements_line(
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(path) = config_path else {
        lines.push(
            "value entitlements: state=disabled enabled=false plans=0 (offline default: value features are denied typed; the local product is unaffected)"
                .into(),
        );
        return;
    };
    match serve_config_and_semantic(Some(path.to_path_buf())) {
        Ok((config, _semantic)) => {
            if !config.billing.enabled {
                lines.push(
                    "value entitlements: state=disabled enabled=false plans=0 (billing disabled: value features are denied typed; the local product is unaffected)"
                        .into(),
                );
                return;
            }
            match config.billing.service_config() {
                Ok(Some(billing)) => {
                    lines.push(format!(
                        "value entitlements: state=enabled enabled=true plans={} default_plan={} (plan-mapped, inference-independent)",
                        billing.plans.len(),
                        billing.default_plan.as_deref().unwrap_or("-")
                    ));
                    for (plan_id, plan) in &billing.plans {
                        let entitlements = plan
                            .value_entitlements()
                            .iter()
                            .map(|entitlement| entitlement.as_str())
                            .collect::<Vec<_>>()
                            .join(",");
                        let governance = plan.governance_capabilities();
                        let mut flags = Vec::new();
                        if governance.sso {
                            flags.push("sso");
                        }
                        if governance.scim {
                            flags.push("scim");
                        }
                        lines.push(format!(
                            "value plan {plan_id}: entitlements={} governance={}",
                            if entitlements.is_empty() {
                                "-"
                            } else {
                                &entitlements
                            },
                            if flags.is_empty() {
                                "-".to_string()
                            } else {
                                flags.join(",")
                            }
                        ));
                    }
                }
                Ok(None) => {
                    lines.push("value entitlements: state=disabled enabled=false plans=0".into())
                }
                Err(e) => {
                    lines.push(format!(
                        "value entitlements: FAILED state=refused enabled=true {e}"
                    ));
                    *issues += 1;
                }
            }
        }
        Err(e) => {
            // The daemon refuses this config wholesale; the worker-plane
            // section above already counts that refusal once.
            lines.push(format!(
                "value entitlements: unavailable state=refused (the daemon refuses this config) {e}"
            ));
        }
    }
}

/// The tokenizer certification section (audit item 22): iterates every
/// BUILT-IN documented catalog row plus (when a config is named) every
/// configured provider's catalog rows, reporting
/// `exact | upper_bound | unsupported` against the ONE tokenizer registry.
/// An exact-accounting row without an exact tokenizer is an ISSUE — the
/// daemon would refuse the configuration — while conservative-accounting
/// rows are reported with the `upper_bound` uncertainty attached to their
/// budget telemetry and stay routable.
pub(crate) fn doctor_tokenizer_certification_line(
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let registry = faktor_context::TokenizerRegistry::with_builtin_backends();
    let mut rows = graph::builtin_tokenizer_rows();
    if let Some(path) = config_path {
        match config::Config::load_strict(path) {
            Ok(cfg) => {
                // Build the configured adapters OFFLINE over a policy-shaped
                // transport (the daemon's own destination policy, no secret
                // scan installed — catalog rows need no request): this
                // discovers each adapter's `known_models()` for the
                // certification matrix without any network activity.
                let sandbox = match cfg.sandbox_policy() {
                    Ok(policy) => policy,
                    Err(e) => {
                        lines.push(format!("tokenizers: FAILED sandbox policy: {e}"));
                        *issues += 1;
                        return;
                    }
                };
                let destinations = sandbox
                    .network
                    .installed()
                    .cloned()
                    .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
                let transport: Arc<dyn faktor_provider::egress::HttpTransport> =
                    match faktor_provider::egress::PolicyCheckedHttpTransport::try_with_policy(
                        destinations,
                    ) {
                        Ok(t) => Arc::new(t),
                        Err(e) => {
                            lines.push(format!("tokenizers: FAILED egress transport: {e}"));
                            *issues += 1;
                            return;
                        }
                    };
                let mut configured = ProviderRegistry::new();
                for p in &cfg.providers {
                    if let Ok(built) = p.build(transport.clone()) {
                        let _ = configured.try_register(built);
                    }
                }
                for id in configured.ids() {
                    if let Some(p) = configured.get(&id) {
                        for model in p.known_models() {
                            rows.push((id.clone(), model));
                        }
                    }
                }
            }
            Err(e) => {
                lines.push(format!(
                    "tokenizers: FAILED config refused before certification: {e}"
                ));
                *issues += 1;
                return;
            }
        }
    }
    match graph::certify_tokenizer_rows(&rows, &registry) {
        Ok(certified) => {
            let exact = certified.iter().filter(|r| r.is_exact()).count();
            let upper = certified
                .iter()
                .filter(|r| {
                    r.compatibility == faktor_provider::catalog::TokenizerCompatibility::UpperBound
                })
                .count();
            let unsupported = certified
                .iter()
                .filter(|r| {
                    r.compatibility == faktor_provider::catalog::TokenizerCompatibility::Unsupported
                })
                .count();
            lines.push(format!(
                "tokenizers: certified {} row(s) exact={exact} upper_bound={upper} \
                 unsupported={unsupported}",
                certified.len()
            ));
            for row in &certified {
                lines.push(format!(
                    "tokenizer: {} {} tokenizer={} compatibility={} accounting={:?} \
                     estimate_kind={} uncertainty={}",
                    row.provider,
                    row.model,
                    row.tokenizer,
                    row.compatibility.as_str(),
                    row.accounting,
                    row.estimate_kind,
                    row.uncertainty.unwrap_or("none"),
                ));
            }
        }
        Err(e) => {
            lines.push(format!("tokenizers: FAILED {e}"));
            *issues += 1;
        }
    }
}

/// The additive CAPABILITY-AUTHORITY section (capability authority law):
/// per configured provider instance and known model, the authority of the
/// capability lookup — `known` (model-specific: probed or declared for the
/// exact id), `declared_generic` (endpoint-declared generic profile:
/// usable, visibly declared) or `unknown` (no probe, no declaration: the
/// runtime GATES tools/vision/context conservatively and warns). Surfaced
/// line-by-line, never repaired here; an `unknown` row names the exact
/// remedy (declare the id in the provider's `models` list or pin a
/// model-specific profile). The section runs only when a config is named,
/// like the tokenizer matrix.
pub(crate) fn doctor_capability_authority_line(
    config_path: Option<&std::path::Path>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
    let Some(path) = config_path else {
        return;
    };
    let cfg = match config::Config::load_strict(path) {
        Ok(cfg) => cfg,
        Err(e) => {
            lines.push(format!(
                "capability authority: unavailable (the daemon refuses this config) {e}"
            ));
            return;
        }
    };
    let sandbox = match cfg.sandbox_policy() {
        Ok(policy) => policy,
        Err(e) => {
            lines.push(format!("capability authority: FAILED sandbox policy: {e}"));
            *issues += 1;
            return;
        }
    };
    let destinations = sandbox
        .network
        .installed()
        .cloned()
        .unwrap_or_else(faktor_security::destination::DestinationPolicy::empty);
    let transport: Arc<dyn faktor_provider::egress::HttpTransport> =
        match faktor_provider::egress::PolicyCheckedHttpTransport::try_with_policy(destinations) {
            Ok(t) => Arc::new(t),
            Err(e) => {
                lines.push(format!(
                    "capability authority: FAILED egress transport: {e}"
                ));
                *issues += 1;
                return;
            }
        };
    let mut configured = ProviderRegistry::new();
    for p in &cfg.providers {
        if let Ok(built) = p.build(transport.clone()) {
            let _ = configured.try_register(built);
        }
    }
    let (mut known, mut declared, mut unknown) = (0usize, 0usize, 0usize);
    for id in configured.ids() {
        let Some(p) = configured.get(&id) else {
            continue;
        };
        for model in p.known_models() {
            let resolution = p.capability_resolution(&model);
            match resolution.authority {
                faktor_core::model::CapabilityAuthority::Known => known += 1,
                faktor_core::model::CapabilityAuthority::DeclaredGeneric => declared += 1,
                faktor_core::model::CapabilityAuthority::Unknown => unknown += 1,
            }
            lines.push(format!(
                "capability authority: {id} {model} authority={} gated={}",
                resolution.authority.as_str(),
                resolution.is_gated()
            ));
        }
    }
    lines.push(format!(
        "capability authority: {known} known, {declared} declared_generic, {unknown} unknown \
         (an unknown row runs with tools/vision/context gated off until the id is declared \
         in the provider's `models` list or a probe establishes it)"
    ));
}

/// The additive engineering-contract compiler self-check (audit item 5): a
/// deterministic compile of a fixture goal + repository snapshot must yield a
/// NON-EMPTY contract with at least one BOUND proof obligation and a
/// byte-identical contract across repeated compiles. A failure is an ISSUE
/// (the daemon's autonomous baseline would be broken), never silence.
pub(crate) fn doctor_contract_compiler_line(lines: &mut Vec<String>, issues: &mut usize) {
    use faktor_core::contract_compile::{
        compile_contract, intent_tokens, ContractCompileRequest, RepositoryFile, RepositorySnapshot,
    };
    use faktor_core::EvidenceBinding;
    let repository = RepositorySnapshot::from_files(vec![
        RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"doctor\"\n"),
        RepositoryFile::with_content(
            "src/auth/refresh.rs",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn replay_is_rejected() {}\n}\n",
        ),
    ]);
    let goal = "Fix the refresh-token race";
    let request = ContractCompileRequest {
        goal: goal.to_string(),
        repository,
    };
    let first = compile_contract(&request);
    let mut files = vec![
        RepositoryFile::with_content("Cargo.toml", "[package]\nname = \"doctor\"\n"),
        RepositoryFile::with_content(
            "src/auth/refresh.rs",
            "#[cfg(test)]\nmod tests {\n    #[test]\n    fn replay_is_rejected() {}\n}\n",
        ),
    ];
    files.reverse();
    let second = compile_contract(&ContractCompileRequest {
        goal: goal.to_string(),
        repository: RepositorySnapshot::from_files(files),
    });
    let bound_obligations = first
        .contract
        .proof_obligations
        .iter()
        .filter(|obligation| obligation.evidence_binding.is_bound())
        .count();
    let selector_bound = first.contract.proof_obligations.iter().any(|obligation| {
        matches!(
            &obligation.evidence_binding,
            EvidenceBinding::TestSelector(_) | EvidenceBinding::TestSelectors(_)
        )
    });
    let deterministic =
        serde_json::to_vec(&first.contract).ok() == serde_json::to_vec(&second.contract).ok();
    let tokens = intent_tokens(goal);
    if !first.contract.is_empty()
        && bound_obligations > 0
        && selector_bound
        && deterministic
        && tokens.contains(&"refresh".to_string())
    {
        lines.push(format!(
            "engineering-contract compiler: native baseline OK state=verified obligations={bound_obligations} selectors_bound=true tokens={} determinism=byte_identical",
            tokens.join(",")
        ));
    } else {
        lines.push(format!(
            "engineering-contract compiler: FAILED state=broken obligations={bound_obligations} selector_bound={selector_bound} determinism={deterministic}"
        ));
        *issues += 1;
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
pub(crate) fn deep_doctor(
    session: &Arc<SessionManager>,
    lines: &mut Vec<String>,
    issues: &mut usize,
) {
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
    // 8b. Durable session wedges (read-only): an op-active session whose
    //     active turn record has no live drive and no resumable record, a
    //     pending permission whose waiter lives only in another process, or
    //     an applied workspace write with neither a verification record for
    //     its task revision nor an integration record. Recovery closes and
    //     expires these at restart; a residue is a wedge doctor must fail
    //     on. The scan is bounded (exact counters, capped detail lines).
    match session.session_wedge_invariants() {
        Ok(s) => {
            lines.push(format!(
                "session wedges: {} active turn(s) without a drive (of {} active), {} ownerless pending permission(s) (of {} pending), {} applied run(s) without verification/integration (of {} applied-write candidate(s))",
                s.active_turns_without_drive,
                s.active_turns,
                s.ownerless_pending_permissions,
                s.pending_permissions,
                s.applied_runs_without_verification,
                s.applied_write_runs,
            ));
            for i in &s.issues {
                lines.push(format!("session wedge [{}]: {}", i.kind, i.detail));
            }
            if s.truncated {
                lines.push(format!(
                    "session wedges: more issues exist than the {} printed detail(s); the counters above are exact",
                    faktor_session::MAX_WEDGE_DETAILS
                ));
            }
            *issues += s.active_turns_without_drive as usize
                + s.ownerless_pending_permissions as usize
                + s.applied_runs_without_verification as usize;
        }
        Err(e) => add_issue(format!("session wedge scan failed: {e}"), lines, issues),
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
    match shared_supervisor() {
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
pub(crate) fn remove_stale_temp_files(
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

/// One rendered session row (shared by the CLI and its scenario tests).
pub(crate) fn session_row_text(r: &faktor_session::SessionHandle) -> String {
    let state = r.state().map(|s| s.label()).unwrap_or("unknown");
    let title = r.title().unwrap_or_default();
    let provider = r.provider().unwrap_or_default();
    let model = r.model().unwrap_or_default();
    format!("{}  {title}  {provider}  {model}  [{state}]", r.id())
}

pub(crate) async fn sessions(data_dir: PathBuf) {
    match SessionManager::open(data_dir.join("store"), data_dir.join("cas"), false) {
        Ok(session) => match session.list_sessions(None) {
            Ok(rows) => {
                for r in rows {
                    println!("{}", session_row_text(&r));
                }
            }
            Err(e) => {
                // A failed listing is a REFUSAL: exit non-zero so scripts
                // (and supervisors) never read a broken store as "no
                // sessions".
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

pub mod daemon;
pub use daemon::*;

/// The bin test target's attributed live-byte cap (CI pipeline 48 was
/// OOM-killed with no attribution). Test-only: the production binary and
/// the library keep the system allocator.
#[cfg(test)]
mod test_memory_guard;

#[cfg(test)]
#[global_allocator]
static TEST_MEMORY_GUARD: test_memory_guard::GuardedAllocator = test_memory_guard::GuardedAllocator;

#[cfg(test)]
mod main_serve_tests;

#[cfg(test)]
mod main_acp_tests;

#[cfg(test)]
mod main_worker_tests;

#[cfg(test)]
mod main_doctor_tests;

#[cfg(test)]
mod main_value_tests;

#[cfg(test)]
mod main_cli_tests;

#[cfg(test)]
mod indep_coding_audit_tests;
