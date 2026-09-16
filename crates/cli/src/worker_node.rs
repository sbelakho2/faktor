//! The additive worker-mode daemon entry (`faktor worker run`): THIS host
//! acting as a remote worker node of a control plane.
//!
//! Disabled parity: with `[worker_node] enabled = false` (the default and the
//! only state of every pre-existing config) the entry refuses typed BEFORE
//! any network or filesystem effect — no plane is contacted, no worker
//! database is created, no job is claimed.
//!
//! When enabled the node:
//!
//! 1. reads the strict `[worker_node]` configuration and its registration
//!    token (`token` inline or `token_payload`, the latter loaded from the
//!    operator-staged payload directory through the strict contract; never
//!    logged);
//! 2. builds the daemon's OWN graph ([`crate::build_daemon`]) — the SAME
//!    session store, agent runtime and provider stack the local path uses
//!    (there is no second engine);
//! 3. registers through the control plane's checked transport
//!    ([`HttpWorkerTransport`] over the daemon's egress transport: every
//!    call passes the request-time destination gate and the outbound secret
//!    scan);
//! 4. long-polls claims (open generations are CAS-accepted; a generation the
//!    scheduler already leased for this worker is adopted), executes each
//!    claimed job in an ISOLATED candidate workspace through
//!    [`LocalPipelineExecutor`] (the daemon's agent, one fresh session per
//!    job, the goal from the staged payload), renews heartbeats while it
//!    runs, submits the digest-bound result with the fail-closed
//!    "not self-verified" claim and cancels/discards on any lease loss.
//!
//! Payload staging: the job payload body is read from
//! `<payload_dir>/<digest>` (default `<data_dir>/worker_payloads`); a missing
//! or oversized payload is fail-closed
//! ([`faktor_worker::runtime::RunOutcome::PayloadUnavailable`]: nothing
//! executes, nothing submits and the lease expires into the bounded requeue
//! policy).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_cloud::SecretToken;
use faktor_provider::egress::{execute_raw, HttpTransport, RawRequest};
use faktor_worker::runtime::{
    ClaimedJob, ExecutionControl, JobExecutionRequest, JobExecutionResult, JobExecutor, JobPayload,
    LoopReport, WorkerRuntime, WorkerRuntimeConfig, WorkerRuntimeError, WorkerTransport,
};
use faktor_worker::{
    HeartbeatOutcome, JobResultOutcome, JobVerificationClaim, RegistrationOutcome, ResultOutcome,
    WorkerCapabilities, WorkerId, WorkerLease, WorkerView, WORKER_PROTOCOL_VERSION,
};

use crate::config::{Config, WorkerNodeCfg};
use crate::payload::PayloadDir;

/// Bound on one staged payload file (the runtime's own payload bound).
const MAX_STAGED_PAYLOAD_BYTES: usize = faktor_worker::MAX_PAYLOAD_BYTES;

fn load_config(data_dir: &Path, config_path: Option<PathBuf>) -> Result<Config, String> {
    let path = config_path.unwrap_or_else(|| data_dir.join("faktor-plus.json"));
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::load(&path).map_err(|e| format!("config {}: {e}", path.display()))
}

/// The registration token: inline or loaded from the operator-staged
/// payload directory (`token_payload`, under `payload_dir`) through the
/// strict contract — regular file, 0600-style on unix, bounded, non-empty.
/// The plaintext never reaches a log line.
fn resolve_token(cfg: &WorkerNodeCfg, data_dir: &Path) -> Result<SecretToken, String> {
    let raw = match (&cfg.token, &cfg.token_payload) {
        (Some(token), None) => token.trim().to_string(),
        (None, Some(name)) => {
            let root = cfg.payload_root(data_dir)?;
            PayloadDir::new(root)
                .load_secret(name)
                .map_err(|e| format!("worker_node: token_payload {e}"))?
        }
        _ => return Err("worker_node: set exactly one of `token` or `token_payload`".into()),
    };
    SecretToken::try_new(raw).map_err(|e| format!("worker_node: {e}"))
}

/// The worker-mode entry. Disabled = typed refusal before any effect.
pub async fn run(
    iterations: Option<u32>,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
) -> Result<(), String> {
    let config = load_config(&data_dir, config_path)?;
    let cfg = config.worker_node.clone();
    if !cfg.enabled {
        return Err(
            "worker node is disabled (enable the [worker_node] section to run it); no claim, no heartbeat, no submission"
                .into(),
        );
    }
    cfg.validate()?;
    let token = resolve_token(&cfg, &data_dir)?;
    let worker_id = cfg.worker_id()?;
    let capabilities = cfg.capabilities()?;
    let base = cfg.control_plane_url()?;
    let display_name = cfg
        .display_name
        .clone()
        .unwrap_or_else(|| worker_id.as_str().to_string());
    let payload_dir = cfg.payload_root(&data_dir)?;

    // The EXISTING local execution machinery: the daemon's own graph.
    let model = config.model.clone();
    let graph = crate::build_daemon(&data_dir, Some(config))?;
    let transport = Arc::new(
        HttpWorkerTransport::new(base, token.clone(), graph.transport.clone())?
            .with_payload_dir(payload_dir),
    );
    let executor = Arc::new(LocalPipelineExecutor::new(
        graph.session.clone(),
        graph.agent.clone(),
        model,
    ));
    let workspace_root = data_dir.join("worker_workspaces");
    std::fs::create_dir_all(&workspace_root)
        .map_err(|e| format!("worker_node: workspace root: {e}"))?;
    let runtime_config = WorkerRuntimeConfig {
        enabled: true,
        worker_id,
        display_name,
        capabilities,
        token,
        claim_deadline_ms: cfg.claim_deadline_ms.unwrap_or(5_000),
        claim_interval_ms: cfg.claim_interval_ms.unwrap_or(250),
        heartbeat_interval_ms: cfg.heartbeat_interval_ms.unwrap_or(15_000),
        discard_workspace_on_success: cfg.discard_workspace_on_success.unwrap_or(true),
    };
    let runtime =
        WorkerRuntime::with_system_services(runtime_config, transport, executor, workspace_root)
            .map_err(|e| e.to_string())?;
    let budget = iterations.unwrap_or(1);
    let report: LoopReport = tokio::task::spawn_blocking(move || runtime.run_loop(budget))
        .await
        .map_err(|e| format!("worker node join: {e}"))?
        .map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::json!({
            "worker": "ran",
            "idle": report.idle,
            "completed": report.completed,
            "superseded": report.superseded,
            "payloadUnavailable": report.payload_unavailable,
            "failed": report.failed,
        })
    );
    Ok(())
}

/// The checked-transport worker client: one blocking method per plane route,
/// driven by a private Tokio runtime (the runtime loop is synchronous and the
/// transport seam is blocking by contract).
pub struct HttpWorkerTransport {
    base: String,
    token: SecretToken,
    transport: Arc<dyn HttpTransport>,
    runtime: tokio::runtime::Runtime,
    payload_dir: std::sync::Mutex<Option<PathBuf>>,
}

impl HttpWorkerTransport {
    pub fn new(
        base: String,
        token: SecretToken,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Self, String> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("worker node http runtime: {e}"))?;
        Ok(Self {
            base,
            token,
            transport,
            runtime,
            payload_dir: std::sync::Mutex::new(None),
        })
    }

    pub fn with_payload_dir(self, dir: PathBuf) -> Self {
        *self.payload_dir.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir);
        self
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, WorkerRuntimeError> {
        let url = format!("{}{}", self.base, path);
        let mut request = RawRequest::new(method, url)
            .header("authorization", format!("Bearer {}", self.token.expose()));
        if let Some(body) = body {
            request = request.json_body(&body);
        }
        let response = self
            .runtime
            .block_on(execute_raw(&*self.transport, request))
            .map_err(|e| WorkerRuntimeError::Transport(e.to_string()))?;
        let status = response.status;
        let text = response.body_text();
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            WorkerRuntimeError::Transport(format!("control plane answered {status}: {e}"))
        })?;
        if !(200..300).contains(&status) {
            let code = value
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(|code| code.as_str())
                .unwrap_or("worker_plane_refused");
            let message = value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .unwrap_or("control plane refused the request");
            let refusal = matches!(
                code,
                "superseded_lease"
                    | "lease_expired"
                    | "lease_not_live"
                    | "worker_revoked"
                    | "protocol_skew"
                    | "result_already_accepted"
                    | "result_conflict"
                    | "worker_saturated"
            );
            let rendered = format!("{code}: {message}");
            return if refusal {
                Err(WorkerRuntimeError::Refused(rendered))
            } else {
                Err(WorkerRuntimeError::Transport(rendered))
            };
        }
        Ok(value)
    }

    fn resolve_payload(&self, digest: &str) -> Option<JobPayload> {
        let dir = self
            .payload_dir
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()?;
        let path = dir.join(digest);
        let metadata = std::fs::metadata(&path).ok()?;
        if metadata.len() == 0 || metadata.len() > MAX_STAGED_PAYLOAD_BYTES as u64 {
            return None;
        }
        let body = std::fs::read_to_string(&path).ok()?;
        let payload = JobPayload {
            media_type: "text/plain".into(),
            body,
        };
        (payload.digest() == digest).then_some(payload)
    }
}

impl WorkerTransport for HttpWorkerTransport {
    fn register(
        &self,
        worker_id: &WorkerId,
        _token: &SecretToken,
        display_name: &str,
        capabilities: &WorkerCapabilities,
    ) -> Result<RegistrationOutcome, WorkerRuntimeError> {
        let value = self.call(
            "POST",
            "/native/workers/register",
            Some(serde_json::json!({
                "token": self.token.expose(),
                "worker_id": worker_id.to_string(),
                "display_name": display_name,
                "capabilities": capabilities,
            })),
        )?;
        let worker: WorkerView = serde_json::from_value(
            value.get("worker").cloned().unwrap_or_default(),
        )
        .map_err(|e| WorkerRuntimeError::Transport(format!("register response decode: {e}")))?;
        Ok(RegistrationOutcome {
            worker,
            capabilities_reconciled: value
                .get("capabilitiesReconciled")
                .and_then(|flag| flag.as_bool())
                .unwrap_or(false),
        })
    }

    fn claim(
        &self,
        _worker_id: &WorkerId,
        _token: &SecretToken,
        _capabilities: &WorkerCapabilities,
    ) -> Result<Option<ClaimedJob>, WorkerRuntimeError> {
        let value = self.call(
            "POST",
            "/native/jobs/claim",
            Some(serde_json::json!({
                "token": self.token.expose(),
                "protocol_version": WORKER_PROTOCOL_VERSION,
            })),
        )?;
        let claimed = value
            .get("claimed")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if claimed.is_null() {
            return Ok(None);
        }
        let job: faktor_worker::ExecutionJob =
            serde_json::from_value(claimed.get("job").cloned().unwrap_or_default())
                .map_err(|e| WorkerRuntimeError::Transport(format!("claim job decode: {e}")))?;
        let lease: WorkerLease =
            serde_json::from_value(claimed.get("lease").cloned().unwrap_or_default())
                .map_err(|e| WorkerRuntimeError::Transport(format!("claim lease decode: {e}")))?;
        let payload = self.resolve_payload(&job.payload_digest);
        Ok(Some(ClaimedJob {
            job,
            lease,
            payload,
        }))
    }

    fn heartbeat(
        &self,
        _job: &faktor_worker::ExecutionJob,
        lease: &WorkerLease,
    ) -> Result<HeartbeatOutcome, WorkerRuntimeError> {
        let value = self.call(
            "POST",
            &format!("/native/workers/{}/heartbeat", lease.worker_id),
            Some(serde_json::json!({
                "token": self.token.expose(),
                "protocol_version": WORKER_PROTOCOL_VERSION,
                "lease_id": lease.lease_id.to_string(),
                "generation": lease.generation.as_u64(),
            })),
        )?;
        let lease_id = value
            .get("leaseId")
            .and_then(|id| id.as_str())
            .ok_or_else(|| WorkerRuntimeError::Transport("heartbeat carries no leaseId".into()))?;
        let lease_id = faktor_worker::WorkerLeaseId::try_new(lease_id.to_string())
            .map_err(|e| WorkerRuntimeError::Transport(e.to_string()))?;
        let generation = value
            .get("generation")
            .and_then(|generation| generation.as_u64())
            .ok_or_else(|| {
                WorkerRuntimeError::Transport("heartbeat carries no generation".into())
            })?;
        let heartbeat_interval_ms = value
            .get("heartbeatIntervalMs")
            .and_then(|interval| interval.as_i64())
            .ok_or_else(|| {
                WorkerRuntimeError::Transport("heartbeat carries no heartbeatIntervalMs".into())
            })?;
        let expires_at_ms = value
            .get("expiresAtMs")
            .and_then(|expires| expires.as_i64())
            .ok_or_else(|| {
                WorkerRuntimeError::Transport("heartbeat carries no expiresAtMs".into())
            })?;
        Ok(HeartbeatOutcome {
            lease_id,
            generation: faktor_worker::JobGeneration(generation),
            heartbeat_interval_ms,
            expires_at_ms,
        })
    }

    fn submit(
        &self,
        job: &faktor_worker::ExecutionJob,
        lease: &WorkerLease,
        digest: &str,
        outcome: JobResultOutcome,
        verification: &JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerRuntimeError> {
        let value = self.call(
            "POST",
            &format!("/native/jobs/{}/result", job.job_id),
            Some(serde_json::json!({
                "token": self.token.expose(),
                "protocol_version": WORKER_PROTOCOL_VERSION,
                "generation": lease.generation.as_u64(),
                "lease_id": lease.lease_id.to_string(),
                "digest": digest,
                "outcome": outcome,
                "verification": verification,
            })),
        )?;
        let outcome = value.get("outcome").cloned().ok_or_else(|| {
            WorkerRuntimeError::Transport("result response carries no outcome".into())
        })?;
        serde_json::from_value(outcome)
            .map_err(|e| WorkerRuntimeError::Transport(format!("result outcome decode: {e}")))
    }
}

/// The host's existing execution machinery behind the worker executor seam:
/// one fresh session in the ISOLATED candidate workspace per job, driven by
/// the daemon's own agent runtime. The result is ALWAYS reported without a
/// self-verification claim (`self_verified: false`, no produced digest): the
/// origin must verify every remote result (fail closed). Cancellation polls
/// the lease-loss flag and abandons the turn.
pub struct LocalPipelineExecutor {
    session: Arc<faktor_session::SessionManager>,
    agent: Arc<faktor_agent::AgentRuntime>,
    runtime: tokio::runtime::Runtime,
    provider: String,
    model: String,
}

impl LocalPipelineExecutor {
    pub fn new(
        session: Arc<faktor_session::SessionManager>,
        agent: Arc<faktor_agent::AgentRuntime>,
        model: String,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("worker node executor runtime");
        Self {
            session,
            agent,
            runtime,
            provider: "default".into(),
            model,
        }
    }
}

impl JobExecutor for LocalPipelineExecutor {
    fn execute(
        &self,
        request: JobExecutionRequest<'_>,
        control: &ExecutionControl,
    ) -> Result<JobExecutionResult, String> {
        let root = request.candidate_root.to_path_buf();
        let goal = request.payload.body.clone();
        let workspace = self
            .session
            .create_workspace(root.to_str().unwrap_or("."))
            .map_err(|e| format!("worker workspace: {e}"))?;
        let row = self
            .session
            .create_session(workspace, "worker-node", &self.provider, &self.model)
            .map_err(|e| format!("worker session: {e}"))?;
        self.runtime.block_on(async {
            let cancelled = async {
                loop {
                    if control.is_cancelled() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            };
            tokio::pin!(cancelled);
            tokio::select! {
                outcome = self.agent.run_turn(row.id(), &goal, &[]) => match outcome {
                    // Fail closed: the remote result never claims a
                    // worker-side verification.
                    Ok(_) => Ok(JobExecutionResult {
                        succeeded: true,
                        self_verified: false,
                        produced_digest: None,
                        detail: String::new(),
                    }),
                    Err(e) => Ok(JobExecutionResult::failure(e.to_string())),
                },
                _ = &mut cancelled => Err("cancelled by lease loss".into()),
            }
        })
    }
}

#[cfg(test)]
mod payload_token_tests {
    //! Adversarial tests of the worker registration-token payload staging:
    //! missing (exact path), world-readable (unix), corrupt and the inline
    //! path, plus the disabled-parity construction that reads nothing.

    use super::*;

    fn node(token_payload: &str) -> WorkerNodeCfg {
        WorkerNodeCfg {
            enabled: true,
            token_payload: Some(token_payload.to_string()),
            ..Default::default()
        }
    }

    fn stage(root: &Path, name: &str, body: &[u8]) {
        std::fs::create_dir_all(root).unwrap();
        let path = root.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn missing_token_payload_refuses_with_the_exact_expected_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve_token(&node("worker.token"), dir.path()).unwrap_err();
        let expected = dir.path().join("worker_payloads").join("worker.token");
        assert!(err.contains(&expected.display().to_string()), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn world_readable_token_payload_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("worker_payloads");
        stage(&root, "worker.token", b"wkr_test-token\n");
        std::fs::set_permissions(
            root.join("worker.token"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let err = resolve_token(&node("worker.token"), dir.path()).unwrap_err();
        assert!(err.contains("0600"), "{err}");
        assert!(!err.contains("wkr_test-token"), "{err}");
    }

    #[test]
    fn corrupt_token_payload_is_refused_typed() {
        let dir = tempfile::tempdir().unwrap();
        stage(
            &dir.path().join("worker_payloads"),
            "worker.token",
            b"two words\n",
        );
        let err = resolve_token(&node("worker.token"), dir.path()).unwrap_err();
        assert!(
            err.contains("whitespace") || err.contains("corrupt"),
            "{err}"
        );
        // A traversal name never reaches the filesystem.
        let err = resolve_token(&node("../escape"), dir.path()).unwrap_err();
        assert!(err.contains("plain bounded file name"), "{err}");
    }

    #[test]
    fn happy_path_loads_the_staged_worker_token_and_trims_one_line_ending() {
        let dir = tempfile::tempdir().unwrap();
        stage(
            &dir.path().join("worker_payloads"),
            "worker.token",
            b"wkr_test-token\n",
        );
        let token = resolve_token(&node("worker.token"), dir.path()).unwrap();
        assert_eq!(token.expose(), "wkr_test-token");
        // The inline path is unchanged and equally strict.
        let inline = WorkerNodeCfg {
            enabled: true,
            token: Some(" wkr_inline ".into()),
            ..Default::default()
        };
        let token = resolve_token(&inline, dir.path()).unwrap();
        assert_eq!(token.expose(), "wkr_inline");
        // Both at once is a refusal (never a silent precedence).
        let both = WorkerNodeCfg {
            enabled: true,
            token: Some("wkr_inline".into()),
            token_payload: Some("worker.token".into()),
            ..Default::default()
        };
        assert!(resolve_token(&both, dir.path())
            .unwrap_err()
            .contains("exactly one"));
    }

    #[test]
    fn disabled_worker_node_reads_no_payload() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = WorkerNodeCfg::default();
        assert!(!cfg.enabled);
        // resolve_token is only reachable after the enabled check in `run`;
        // a disabled config still resolves nothing and creates no directory.
        let _ = cfg.payload_root(dir.path()).unwrap();
        assert!(!dir.path().join("worker_payloads").exists());
    }
}
