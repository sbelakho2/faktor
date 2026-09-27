//! `api::deps`: cohesive slice of the api module.

use super::*;

/// Handle to the daemon's evidence store: an evidence store behind a
/// process-wide lock, so the server can hold it while producers (future
/// audit) insert. Constructed through [`empty_evidence_store`] in hosts
/// that do not yet run a capture path.
pub type EvidenceStoreHandle =
    Arc<std::sync::RwLock<Box<dyn faktor_evidence::store::EvidenceStore + Send + Sync>>>;

/// The default empty evidence store: an in-memory store that retains
/// bounded backing bytes. `/native/evidence/{id}` answers an honest 404
/// until a producer inserts, never a fabricated envelope.
pub fn empty_evidence_store() -> EvidenceStoreHandle {
    Arc::new(std::sync::RwLock::new(Box::new(
        faktor_evidence::store::MemoryEvidenceStore::new(4 * 1024 * 1024),
    )))
}

/// Typed construction refusal of [`ServerDeps::new`]: the embedded host's
/// session store carries a degenerate data root, so the daemon-owned shadow
/// root cannot be derived without a silent fallback to a
/// process-working-directory-relative path. No such fallback exists — the
/// host must open its store at an explicit data root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServerDepsError {
    #[error("cannot derive the server shadow root from the session store data root: {0}")]
    DegenerateShadowRoot(#[from] faktor_orchestrator::runtime::shadow::ShadowRootError),
}

pub struct ServerDeps {
    pub session: Arc<SessionManager>,
    pub agent: Arc<AgentRuntime>,
    pub permissions: Arc<ChannelPermissionRequester>,
    /// The orchestration runtime (audits P0-20/21/23/61): the AUTHORITATIVE
    /// executor of multi-agent tasks and the durable control surface the
    /// `/native/agents/{child}/...` endpoints drive. Non-optional in
    /// production: the CLI wires the real runtime in the daemon graph
    /// region; `ServerDeps::new` (tests) builds one over the same
    /// session+agent.
    pub orchestrator: Arc<faktor_orchestrator::runtime::OrchestratorRuntime>,
    /// The TaskExecutor (audits P0-20/21/23/61/90/91): ONE entry for native
    /// task starts — single-item tasks drive the existing session with the
    /// daemon's own prompt path, multi-item tasks spawn real children
    /// through [`ServerDeps::orchestrator`].
    pub tasks: Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
    /// The daemon's durable cost ledger (audit 12/17): the SAME ledger the
    /// graph built. Handlers that touch per-task/per-child caps use this
    /// authority instead of constructing a second ledger per request.
    pub budgets: Arc<faktor_session::DurableBudgetLedger>,
    /// Per-start bearer token (legacy clients); the frontend uses the
    /// password. Both ride `Authorization: Bearer`.
    pub auth_token: AuthToken,
    /// The password the frontend generated and passed via `FAKTOR_SERVER_PASSWORD`.
    pub server_password: ServerPassword,
    /// The workspace root carried on the server's dependency envelope.
    pub directory: Option<String>,
    pub version: String,
    /// Real workspace file service for revert/unrevert/diff (None = the wire
    /// surface refuses with an honest 409).
    pub fs: Option<Arc<faktor_fs::WorkspaceFileService>>,
    /// Real checkpoint store for revert/unrevert/diff (None = honest 409).
    pub snapshots: Option<Arc<faktor_snapshot::CheckpointStore>>,
    /// The daemon's evidence store (audit 82): scope-checked reads of
    /// captured evidence envelopes for the native evidence endpoints.
    /// `None` = no store wired in this host (the endpoints answer 503).
    pub evidence: Option<EvidenceStoreHandle>,
    /// The semantic provider registry (audit 83) surfaced by
    /// `/native/semantic/{status,capabilities}`. `None` = no provider is
    /// configured; the endpoints report the fallback registry shape.
    pub semantic: Option<Arc<faktor_semantic::registry::SemanticProviderRegistry>>,
    /// Live chunk stream from the agent (audit round 11): when present,
    /// serve() drains it into low-latency session.next.*.delta frames.
    /// Bounded (audit 41): the agent's [`faktor_agent::ChunkSink`] sender
    /// half coalesces ephemeral deltas under backpressure instead of
    /// growing memory; this receiver half stays drained eagerly into the
    /// bounded global ring.
    pub chunk_rx: Option<tokio::sync::mpsc::Receiver<faktor_agent::ChunkEvent>>,
    /// Deterministic readiness knob (audit 55; mirrors the suggested
    /// `FAKTOR_SIMULATE_NOT_READY=1` gate as a field — an env gate would
    /// race parallel tests in one process). When true, the ready flag stays
    /// false after serve() setup, so `GET /native/ready` keeps answering
    /// 503 `{"ready":false}`. Production callers never set it.
    pub simulate_not_ready: bool,
    /// The control-plane service (identity/organizations/RBAC). `None` = the
    /// `[cloud]` section is disabled (the default): every control-plane
    /// route answers a typed 409 `cloud_disabled` and nothing else in the
    /// daemon changes.
    pub control_plane: Option<Arc<faktor_cloud::ControlPlane>>,
    /// The durable SCM store serving `/native/repositories`. `None` = no
    /// SCM state is wired (the route answers a typed 409 `scm_disabled`).
    pub scm: Option<Arc<dyn faktor_scm::ScmStore>>,
    /// The wired SCM webhook sink (`POST /native/scm/webhook`): the
    /// verified inbox claim plus the idempotent re-sync scheduling. `None` =
    /// no GitHub App is wired (the default): the route answers a typed 409
    /// `scm_webhook_disabled` and the daemon is otherwise byte-identical.
    pub scm_webhook: Option<Arc<dyn crate::native::WebhookSink>>,
    /// The Wave 3 commercial metering/entitlement service. `None` = the
    /// `[billing]` section is disabled (the default): every billing route
    /// answers a typed 409 `billing_disabled` and the daemon is otherwise
    /// byte-identical to the pre-billing daemon.
    pub billing: Option<Arc<faktor_cloud::EntitlementService>>,
    /// The signed updater service (`[updater]`). `None` = the section is
    /// disabled (the default): every `/native/updater/*` route answers a
    /// typed 409 `updater_disabled` and the daemon is otherwise byte-
    /// identical to the pre-updater daemon.
    pub updater: Option<Arc<faktor_updater::Updater>>,
    /// The remote/VPC worker plane (`[workers]`). `None` = the section is
    /// disabled (the default): every `/native/workers*` and
    /// `/native/jobs/*` route answers a typed 409 `workers_disabled`, the
    /// daemon creates no worker database file, and the TaskExecutor's
    /// placement seam stays disabled (local execution unchanged).
    pub workers: Option<Arc<faktor_worker::WorkerPlane>>,
    /// The enterprise plane service (`[enterprise]`): retention classes/GC,
    /// the audit ledger, admin settings and deletion jobs. `None` = the
    /// section is disabled (the default): every `/native/enterprise/*` route
    /// answers a typed 409 `enterprise_disabled` and the daemon is otherwise
    /// byte-identical to the pre-enterprise daemon.
    pub enterprise: Option<Arc<faktor_cloud::EnterpriseService>>,
    /// The daemon-side retention runtime (session store + CAS) the GC route
    /// scans and deletes through. `None` = the GC route answers a typed 409
    /// `retention_runtime_disabled` (the other enterprise routes still work
    /// on durable metadata only).
    pub retention: Option<Arc<crate::native::enterprise::RetentionRuntime>>,
    /// The SSO login authority (`/native/sso/{start,callback}`) over the
    /// configured network OIDC adapter. `None` = SSO is disabled (the
    /// default): both routes answer a typed 409 `sso_disabled` and the daemon
    /// is otherwise byte-identical.
    pub sso: Option<Arc<faktor_cloud::SsoLogin>>,
    /// The dedicated worker-plane listener slot (see [`WorkerPlaneListener`]).
    /// `None` = the `[worker_plane]` section is disabled (the default): the
    /// native health payload reports an explicit `disabled` worker-plane
    /// state and no second socket exists.
    pub worker_plane_listener: Option<WorkerPlaneListener>,
    /// The terminal execution-authority policy: the ONE admission gate every
    /// session-owned terminal spawn passes through. The host CONSTRUCTS it
    /// from its configured `[sandbox]` shell contract
    /// ([`TerminalAuthorityPolicy::for_configured_sandbox`]) — the configured
    /// `ShellExecutionMode`/isolation policy is carried verbatim and is what
    /// the authority enforces AND records in the durable execution profile.
    /// The field default is the fail-closed `os_isolated`/`Required` shape:
    /// an embedded host that injects nothing refuses PTY spawns typed rather
    /// than running an ungated shell.
    pub terminal_policy: TerminalAuthorityPolicy,
}

impl ServerDeps {
    /// Default construction (embedded hosts + tests): builds a runtime over
    /// the same session+agent. The daemon NEVER uses this path for its
    /// production surface — `serve` assembles `ServerDeps` from the named
    /// daemon graph's instances through [`ServerDeps::new_with`], so no
    /// second orchestrator/executor is ever constructed in a daemon
    /// lifetime (audit 12/17).
    ///
    /// Fails TYPED when the session store carries a degenerate data root: the
    /// embedded host carries the SAME isolation authority the daemon graph
    /// does (the shadow service rooted beside the store's data dir), and the
    /// root derives from the store's EXPLICIT data root
    /// (`faktor_store::Store::root`, the directory the store was opened at).
    /// An empty root would derive the bare relative `shadows` path and
    /// silently write under the process working directory, so it is a typed
    /// [`ServerDepsError`] instead — there is NO fallback and no embedded
    /// host or test can inherit a shared `/tmp/faktor-shadows` production
    /// path.
    pub fn new(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        permissions: Arc<ChannelPermissionRequester>,
    ) -> Result<Self, ServerDepsError> {
        let orchestrator =
            faktor_orchestrator::runtime::OrchestratorRuntime::new(session.clone(), agent.clone());
        let shadows_root = session.store().root().join("shadows");
        let shadows =
            faktor_orchestrator::runtime::shadow::ShadowRoots::new(session.clone(), shadows_root)?;
        let tasks = faktor_orchestrator::runtime::task_executor::TaskExecutor::new(
            &orchestrator,
            session.clone(),
            agent.clone(),
            shadows,
        );
        let budgets = faktor_session::DurableBudgetLedger::new(session.clone());
        Ok(Self::new_with(
            session,
            agent,
            permissions,
            orchestrator,
            tasks,
            budgets,
        ))
    }

    /// Assemble the server surface over GRAPH-PROVIDED runtime authorities
    /// (audit 12/17): production passes the daemon graph's orchestrator,
    /// TaskExecutor and budget ledger — the SAME instances the rest of the
    /// daemon uses — so the server never constructs a second execution or
    /// money authority.
    pub fn new_with(
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        permissions: Arc<ChannelPermissionRequester>,
        orchestrator: Arc<faktor_orchestrator::runtime::OrchestratorRuntime>,
        tasks: Arc<faktor_orchestrator::runtime::task_executor::TaskExecutor>,
        budgets: Arc<faktor_session::DurableBudgetLedger>,
    ) -> Self {
        Self {
            session,
            agent,
            permissions,
            orchestrator,
            tasks,
            budgets,
            auth_token: AuthToken::generate(),
            // The CLI startup path overrides this with the frontend-generated
            // `FAKTOR_SERVER_PASSWORD` via `ServerPassword::try_from_env`
            // (which fails loudly on a malformed explicit value). The library
            // default is a fresh ephemeral 256-bit secret — never a weak or
            // empty password.
            server_password: ServerPassword::generate(),
            directory: None,
            version: faktor_core::VERSION.to_string(),
            fs: None,
            snapshots: None,
            evidence: None,
            semantic: None,
            chunk_rx: None,
            simulate_not_ready: false,
            control_plane: None,
            scm: None,
            scm_webhook: None,
            billing: None,
            updater: None,
            workers: None,
            enterprise: None,
            retention: None,
            sso: None,
            worker_plane_listener: None,
            terminal_policy: TerminalAuthorityPolicy::default(),
        }
    }

    /// Wire the terminal execution-authority policy (the configured
    /// `[sandbox]` shell contract) so every session-owned terminal spawn is
    /// admitted, enforced and recorded under it. Additive: without it the
    /// fail-closed `os_isolated` default governs.
    pub fn with_terminal_policy(mut self, policy: TerminalAuthorityPolicy) -> Self {
        self.terminal_policy = policy;
        self
    }

    /// Wire the real native snapshot store so `/session/{id}/revert`,
    /// `/unrevert` and `/diff` actually restore files. Both must be provided
    /// together; with `None` the endpoints keep their honest 409.
    pub fn with_snapshots(
        mut self,
        fs: Arc<faktor_fs::WorkspaceFileService>,
        snapshots: Arc<faktor_snapshot::CheckpointStore>,
    ) -> Self {
        self.fs = Some(fs);
        self.snapshots = Some(snapshots);
        self
    }

    /// Wire the daemon's evidence store so the native evidence endpoints can
    /// serve scope-checked reads. Production wires the graph's store here;
    /// embedded hosts leave it `None` and answer an honest 503.
    pub fn with_evidence_store(mut self, store: EvidenceStoreHandle) -> Self {
        self.evidence = Some(store);
        self
    }

    /// Wire the semantic provider registry surfaced by the native semantic
    /// introspection endpoints. `None` reports the fallback-only shape.
    pub fn with_semantic_registry(
        mut self,
        registry: Arc<faktor_semantic::registry::SemanticProviderRegistry>,
    ) -> Self {
        self.semantic = Some(registry);
        self
    }

    /// Wire the control-plane service (the `[cloud]` section). Additive:
    /// without it every control-plane route answers 409 `cloud_disabled`.
    pub fn with_control_plane(mut self, control_plane: Arc<faktor_cloud::ControlPlane>) -> Self {
        self.control_plane = Some(control_plane);
        self
    }

    /// Wire the durable SCM store serving `/native/repositories`.
    pub fn with_scm_store(mut self, store: Arc<dyn faktor_scm::ScmStore>) -> Self {
        self.scm = Some(store);
        self
    }

    /// Wire the SCM webhook sink (`POST /native/scm/webhook`). Additive:
    /// without it the route answers a typed 409 `scm_webhook_disabled`.
    pub fn with_scm_webhook(mut self, sink: Arc<dyn crate::native::WebhookSink>) -> Self {
        self.scm_webhook = Some(sink);
        self
    }

    /// Wire the Wave 3 commercial metering service (`[billing]`). Additive:
    /// without it every billing route answers 409 `billing_disabled`.
    pub fn with_billing(mut self, billing: Arc<faktor_cloud::EntitlementService>) -> Self {
        self.billing = Some(billing);
        self
    }

    /// Wire the signed updater service (`[updater]`). Additive: without it
    /// every `/native/updater/*` route answers 409 `updater_disabled`.
    pub fn with_updater(mut self, updater: Arc<faktor_updater::Updater>) -> Self {
        self.updater = Some(updater);
        self
    }

    /// Wire the SSO login authority (`/native/sso/{start,callback}`) over the
    /// configured network OIDC adapter. Additive: without it both routes
    /// answer a typed 409 `sso_disabled`.
    pub fn with_sso(mut self, sso: Arc<faktor_cloud::SsoLogin>) -> Self {
        self.sso = Some(sso);
        self
    }

    /// Wire the remote/VPC worker plane (`[workers]`). Additive: without it
    /// every worker/job route answers 409 `workers_disabled` and the
    /// TaskExecutor's placement seam stays disabled (local parity).
    pub fn with_workers(mut self, workers: Arc<faktor_worker::WorkerPlane>) -> Self {
        self.workers = Some(workers);
        self
    }

    /// Wire the dedicated worker-plane listener slot so the native health
    /// payload reports its typed liveness (additive `worker_plane` key).
    /// Without it the payload reports an explicit `disabled` state.
    pub fn with_worker_plane_listener(mut self, listener: WorkerPlaneListener) -> Self {
        self.worker_plane_listener = Some(listener);
        self
    }

    /// The typed health of the dedicated worker-plane listener (the additive
    /// `/native/health` entry). A poisoned slot lock degrades to the
    /// recorded state rather than panicking a health probe.
    pub fn worker_plane_health(&self) -> WorkerPlaneHealth {
        match &self.worker_plane_listener {
            None => WorkerPlaneHealth::Disabled,
            Some(listener) => {
                let slot = listener
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                WorkerPlaneHealth::Enabled {
                    bind: slot.bind(),
                    status: slot.status(),
                }
            }
        }
    }

    /// Wire the enterprise plane (`[enterprise]`) and, optionally, the
    /// daemon-side retention runtime (session store + CAS) the GC route
    /// scans and deletes through. Additive: without the service every
    /// enterprise route answers 409 `enterprise_disabled`.
    pub fn with_enterprise(
        mut self,
        enterprise: Arc<faktor_cloud::EnterpriseService>,
        retention: Option<Arc<crate::native::enterprise::RetentionRuntime>>,
    ) -> Self {
        self.enterprise = Some(enterprise);
        self.retention = retention;
        self
    }

    /// The startup line the CLI prints on stdout after binding.
    pub fn startup_line(&self, addr: SocketAddr) -> String {
        startup_line(addr.port())
    }
}

#[cfg(test)]
#[path = "tests.rs"]
pub(crate) mod tests;

#[cfg(test)]
#[path = "deps_tests.rs"]
mod deps_tests;
