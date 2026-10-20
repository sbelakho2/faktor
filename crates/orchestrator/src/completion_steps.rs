//! PR/CI-fix completion step EXECUTION (the P2 follow-up to the durable
//! completion-contract gate in `faktor-core`/`faktor-session`).
//!
//! [`CompletionStepRunner`] executes the conditional steps a task run's
//! ACCEPTED durable completion contract requests — in gate order
//! (commit, push, pr) — against the run's integration/candidate root, and
//! records every outcome through the SAME durable setter the gate reads
//! ([`SessionHandle::set_completion_step_status`]). Nothing here is a second
//! completion authority: after the runner, the existing
//! `complete_verified_task` gate is asked to complete and refuses exactly
//! as before (missing -> `CompletionStepMissing`, non-succeeded ->
//! `CompletionStepNotSucceeded`, any `Failed` row -> `CompletionStepFailed`,
//! terminal).
//!
//! Ordering (normative): deterministic verification FIRST, then these steps,
//! then the completion gate. The orchestrator invokes the runner only AFTER
//! a run's drive has ended (verification ran) and records outcomes durably
//! BEFORE any later completion attempt. A run with no contract — or the
//! all-false default — NEVER constructs or invokes the runner: the default
//! path stays byte-identical.
//!
//! Semantics per step:
//!
//! - **commit**: stage every change in the task's integration/candidate root
//!   and `git commit` on the current branch, with a bounded message derived
//!   from the goal. A clean tree is honestly `Skipped` ("nothing to
//!   commit"); an unborn HEAD (empty repository) is `Skipped` too — this
//!   path never mints an empty or initial commit. A clean tree whose HEAD
//!   already carries the SAME deterministic message (a crash between the
//!   commit and its status row) records `Succeeded` ("already committed").
//! - **push**: push the current branch to the configured remote through the
//!   workspace's git manager, consulting the injected egress policy for any
//!   real network destination (local path / `file://` remotes are not
//!   egress). No remote configured => `Skipped` recorded; policy denial =>
//!   `Failed` with the typed reason; an already-pushed HEAD (local
//!   remote-tracking ref equal to HEAD, or git's "Everything up-to-date")
//!   => `Succeeded` with a note, so retries are idempotent.
//! - **pr** (legacy/test path): create the PR ONLY when `pr_command` is
//!   configured (strict config; templated with `{branch}`/`{base}`/`{remote}`;
//!   executed through the process supervisor with a sanitized baseline
//!   environment and bounded captured output — never a direct network call).
//!   Unconfigured => `Skipped`; output naming a PR URL => `Succeeded` with
//!   the parsed URL; a command that reports the PR "already exists" =>
//!   `Succeeded` (idempotent) with the URL when present. This path records
//!   NO external-operation identity and therefore never certifies the native
//!   PR step in production; see the typed path above.
//!
//! - **pr**: the native PR step is CERTIFIED only through the canonical
//!   [`faktor_scm::CompletionScm`] adapter and its typed reconciliation
//!   protocol: the exact operation identity (installation/repository +
//!   head/base + a stable Faktor task marker) is journaled as a durable
//!   [`ExternalOperationRow`] BEFORE the remote call, and the remote object
//!   id + version are journaled after it. A crash mid-operation reconciles
//!   from the RECORDED identity through the adapter — never a blind
//!   re-create, never a duplicate remote object; a conflicting recorded
//!   input identity and a remote-object version drift are typed refusals.
//!   The adapter maps the request onto the canonical `faktor_scm`
//!   [`faktor_scm::ScmProvider`] types; production wiring uses the real
//!   GitHub App adapter (`faktor_scm::github::GitHubApp`) over the daemon's
//!   ONE checked transport, so there is no second SCM domain here. The
//!   generic `pr_command`/`pr_program` expert command remains executable on
//!   the legacy path but carries NO reconciliation protocol, so it can never
//!   certify the native PR step ([`PR_REQUIRES_TYPED_RECONCILIATION`]).
//!   A contract that requests the PR step while NO SCM provider is wired is
//!   an explicit configuration blocker ([`NATIVE_PR_SCM_NOT_CONFIGURED`]):
//!   the step is never silently skipped and never rebuilt without the
//!   provider.
//!
//! Stop rule: only a `Failed` step stops the ordered execution (terminal for
//! the contract revision). Remaining requested steps are recorded `Skipped`
//! ("not attempted: <step> failed") and are never executed. A `Skipped` step
//! (e.g. nothing to commit, no remote, unconfigured PR) does NOT stop the
//! sequence but is still an unmet step for the gate: it stays retryable.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::runtime::merge::MAX_BASE_ENTRIES;
use faktor_core::cancellation::CancellationToken;
use faktor_core::completion::{CompletionStep, CompletionStepOutcome};
use faktor_core::id::{TaskId, VerificationRecordId};
use faktor_git::{CommitOutcome, PushOutcome, VerifiedTreeEntry, WorktreeManager};
use faktor_scm::{CompletionPrRequest, CompletionScm};
use faktor_session::ledger::{
    DurableRead, ExternalOperationInput, ExternalOperationRow, ExternalOperationState,
    VerifiedGitArtifact, VerifiedManifestEntry,
};
use faktor_session::{SessionHandle, MAX_COMPLETION_STEP_DETAIL};
use faktor_terminal::{EnvSpec, ProcessOwner, ProcessSupervisor, SpawnConfig};

/// Hard bound on the goal-derived commit message (UTF-8 bytes).
pub const MAX_COMMIT_MESSAGE_BYTES: usize = 200;
/// Hard bound on one configured PR command template (and on ONE typed argv
/// element: the program or one argument).
pub const MAX_PR_COMMAND_BYTES: usize = 4096;
/// Hard bound on the number of typed `pr_args` elements.
pub const MAX_PR_ARGS: usize = 128;
/// Bound on the captured PR command excerpt put into the durable detail.
pub const MAX_PR_OUTPUT_BYTES: usize = 2000;
/// Deliberate bound on one PR command (a wedged helper must not hang the
/// daemon; the supervisor kills it and the step fails typed).
pub const PR_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The strict execution configuration of the completion-step runner. The
/// daemon maps its `[completion]` config section onto this type; the
/// defaults are intentionally inert (no PR command), so an unconfigured
/// daemon records the documented `Skipped` outcomes.
///
/// Two PR shapes are accepted:
/// - `pr_program` + `pr_args` (P3, preferred): a typed argv executed
///   directly through the supervisor — no shell, no whitespace splitting, so
///   quoted arguments and paths with spaces are representable. Every element
///   substitutes `{branch}`/`{base}`/`{remote}` independently.
/// - `pr_command` (DEPRECATED): the historic whitespace-split string
///   template, kept working with the exact same security checks. It is
///   mutually exclusive with the typed shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionStepsConfig {
    /// The git remote the push step targets.
    pub remote: String,
    /// The base branch rendered into the PR template.
    pub base_branch: String,
    /// DEPRECATED: the whitespace-split PR command template. `None` = the
    /// documented "not configured" `Skipped` outcome for a requested PR
    /// step (unless `pr_program` is configured).
    #[serde(default)]
    pub pr_command: Option<String>,
    /// The typed argv program (executed directly; spaces are preserved).
    /// `None` with `pr_command: None` = the documented `Skipped` outcome.
    #[serde(default)]
    pub pr_program: Option<String>,
    /// The typed argv arguments; each element is placeholder-substituted.
    #[serde(default)]
    pub pr_args: Vec<String>,
}

impl Default for CompletionStepsConfig {
    fn default() -> Self {
        Self {
            remote: "origin".into(),
            base_branch: "main".into(),
            pr_command: None,
            pr_program: None,
            pr_args: Vec::new(),
        }
    }
}

impl CompletionStepsConfig {
    /// Strict validation: bounded, option-shaped remote and base branch; a
    /// PR command that is bounded, single-line and templated only with the
    /// documented placeholders and at least `{branch}` (a command that
    /// cannot name the PR head is refused). The legacy string template
    /// additionally refuses shell metacharacters (it is split on
    /// whitespace); the typed argv shape allows every non-control character
    /// because no shell and no splitting is ever involved. Configuring BOTH
    /// shapes is ambiguous and refused.
    pub fn validate(&self) -> Result<(), String> {
        validate_remote_name(&self.remote)?;
        validate_branch_name(&self.base_branch)?;
        match (&self.pr_command, &self.pr_program) {
            (Some(_), Some(_)) => Err(
                "pr_command (deprecated) and pr_program are mutually exclusive; configure exactly one"
                    .into(),
            ),
            (Some(command), None) => validate_pr_command(command),
            (None, Some(program)) => validate_pr_argv(program, &self.pr_args),
            (None, None) if self.pr_args.is_empty() => Ok(()),
            (None, None) => Err("pr_args requires pr_program".into()),
        }
    }
}

/// The egress decision seam the push step consults for network remotes. The
/// daemon wires its sandbox `PermissionEngine::check_egress` here; tests
/// inject allow/deny policies. Local-path and `file://` remotes are NOT
/// egress and never consult this policy.
pub trait EgressPolicy: Send + Sync {
    /// `Ok(())` allows the destination; `Err(reason)` is the typed denial.
    fn check(&self, destination: &str) -> Result<(), String>;
}

impl<F> EgressPolicy for F
where
    F: Fn(&str) -> Result<(), String> + Send + Sync,
{
    fn check(&self, destination: &str) -> Result<(), String> {
        self(destination)
    }
}

/// The stable machine tag of the native-PR typed gate: a generic
/// `pr_command`/`pr_program` expert command implements NO typed
/// reconciliation protocol, so it can never certify the native PR step (the
/// typed refusal detail always carries this code).
pub const PR_REQUIRES_TYPED_RECONCILIATION: &str = "pr_requires_typed_reconciliation_protocol";

/// The stable machine code of the explicit configuration blocker recorded
/// when a task's completion contract requests the native PR step but no SCM
/// provider is wired: fail closed with the exact missing configuration,
/// never a silent `Skipped` and never a rebuild without the provider.
pub const NATIVE_PR_SCM_NOT_CONFIGURED: &str = "native_pr_scm_not_configured";

/// The durable external-operation kind of the native pull-request
/// certification.
pub const SCM_PULL_REQUEST_KIND: &str = "pull_request";

// -------------------------------------------------- typed SCM reconciliation

/// Typed refusal of the durable external-operation protocol. Every variant
/// carries a stable machine code into the durable step detail, so a refused
/// PR step is traceable to the exact reconciliation rule it violated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExternalOperationError {
    #[error(
        "external operation {operation_key} was recorded with a different input identity: {detail}"
    )]
    InputIdentityConflict {
        operation_key: String,
        detail: String,
    },
    #[error("external operation {operation_key} remote object mismatch: {detail}")]
    RemoteObjectMismatch {
        operation_key: String,
        detail: String,
    },
    #[error("external operation {operation_key} is terminally failed: {detail}")]
    RecordedFailure {
        operation_key: String,
        detail: String,
    },
    #[error("external operation {operation_key} store is unavailable: {detail}")]
    Store {
        operation_key: String,
        detail: String,
    },
}

impl ExternalOperationError {
    /// The stable machine code carried into the durable step detail.
    pub const fn code(&self) -> &'static str {
        match self {
            ExternalOperationError::InputIdentityConflict { .. } => {
                "external_operation_input_identity_conflict"
            }
            ExternalOperationError::RemoteObjectMismatch { .. } => {
                "external_operation_remote_object_mismatch"
            }
            ExternalOperationError::RecordedFailure { .. } => "external_operation_recorded_failure",
            ExternalOperationError::Store { .. } => "external_operation_store_unavailable",
        }
    }
}

/// The stable Faktor task marker a provider-side reconciliation lookup is
/// keyed by. Bounded, deterministic and scoped to the contract revision, so
/// the same task+revision can never mint a second marker.
pub fn native_pr_marker(task_id: TaskId, revision: u64) -> String {
    format!("faktor:task:{}:rev:{}", task_id.raw(), revision)
}

/// The stable durable operation key of one native PR operation: exactly one
/// operation per (task, contract revision, provider, kind).
pub fn native_pr_operation_key(task_id: TaskId, revision: u64, provider: &str) -> String {
    format!(
        "task:{}:rev:{}:{}:{SCM_PULL_REQUEST_KIND}",
        task_id.raw(),
        revision,
        provider
    )
}

/// Parse one git remote URL into a GitHub repository reference
/// (`organization`, `repository`; `None` for anything else — the typed path
/// refuses an unrecognizable remote instead of guessing a pair). Accepts
/// `https://github.com/org/repo[.git]`, `ssh://git@github.com/org/repo` and
/// the scp-like `git@github.com:org/repo[.git]`. The installation identity
/// is resolved by the SCM adapter from the durable synced rows, never
/// guessed from the URL.
pub fn parse_scm_repository(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    let (host, path) = if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string())
    } else {
        let (scheme, rest) = url.split_once("://")?;
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return None;
        }
        let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.to_string(), path.to_string())
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path.as_str());
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() != 2 {
        return None;
    }
    let organization = segments[0];
    let repository = segments[1].strip_suffix(".git").unwrap_or(segments[1]);
    if organization.is_empty() || repository.is_empty() {
        return None;
    }
    Some((organization.to_string(), repository.to_string()))
}

/// One task-run execution context: where the steps run and what the commit
/// message / PR template derive from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionStepContext {
    /// The task's integration/candidate root (the live shadow root for a
    /// shadowed run, else the session's durable owner worktree root).
    pub root: PathBuf,
    /// The task goal (the commit message derives from it, bounded).
    pub goal: String,
}

/// One recorded step outcome (mirrors the durable row plus its seq).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionStepRecord {
    pub step: CompletionStep,
    pub status: CompletionStepOutcome,
    pub detail: String,
    /// The durable row seq when THIS invocation appended one; `None` for an
    /// idempotent replay of an existing row or a terminal pre-existing
    /// failure (nothing new is written).
    pub seq: Option<i64>,
}

/// The outcome of one runner invocation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CompletionStepReport {
    /// Every requested step, in gate order, with its durable outcome.
    pub records: Vec<CompletionStepRecord>,
    /// The PR URL parsed from the configured command's output, when any.
    pub pr_url: Option<String>,
}

impl CompletionStepReport {
    /// The requested steps in execution order.
    pub fn requested(&self) -> Vec<CompletionStep> {
        self.records.iter().map(|r| r.step).collect()
    }

    /// The latest outcome of one step in this report.
    pub fn outcome_of(&self, step: CompletionStep) -> Option<CompletionStepOutcome> {
        self.records
            .iter()
            .rev()
            .find(|r| r.step == step)
            .map(|r| r.status)
    }

    /// True when every requested step recorded `Succeeded`.
    pub fn all_succeeded(&self) -> bool {
        self.records
            .iter()
            .all(|r| r.status == CompletionStepOutcome::Succeeded)
    }

    /// The first failed step, if any.
    pub fn failed_step(&self) -> Option<CompletionStep> {
        self.records
            .iter()
            .find(|r| r.status == CompletionStepOutcome::Failed)
            .map(|r| r.step)
    }

    /// The first invalidated step, if any (the verified root moved; the run
    /// must be re-integrated and re-verified before the step may run).
    pub fn invalidated_step(&self) -> Option<CompletionStep> {
        self.records
            .iter()
            .find(|r| r.status == CompletionStepOutcome::Invalidated)
            .map(|r| r.step)
    }
}

/// Typed failure of the runner's OWN plumbing (contract/ledger reads, config
/// render). Per-step outcomes are never errors: they are recorded durably as
/// `Failed` rows (the gate's terminal refusal), not returned as `Err`.
#[derive(Debug, thiserror::Error)]
pub enum CompletionStepError {
    #[error("completion step session read: {0}")]
    Session(#[from] faktor_session::TaskError),
    #[error("completion step ledger read: {0}")]
    Ledger(#[from] faktor_core::Error),
    #[error("completion step config: {0}")]
    Config(String),
    /// Test-only deterministic crash seam of the typed PR operation: the
    /// exact boundary a real daemon crash would hit (the durable rows stay
    /// exactly as they were at that instant).
    #[cfg(test)]
    #[error("injected completion-step crash at {0}")]
    InjectedCrash(&'static str),
}

/// Deterministic crash seam of the typed PR operation (tests only).
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrOperationCrashPoint {
    /// After the durable `Prepared` row, BEFORE the remote call.
    BeforeRemoteCall,
    /// After the remote call returned, BEFORE the durable `Completed` row.
    AfterRemoteCallBeforeRecord,
}

/// What one step execution produced.
enum StepExecution {
    Succeeded {
        detail: String,
        pr_url: Option<String>,
    },
    Skipped {
        detail: String,
    },
    Failed {
        detail: String,
    },
}

impl StepExecution {
    fn status(&self) -> CompletionStepOutcome {
        match self {
            StepExecution::Succeeded { .. } => CompletionStepOutcome::Succeeded,
            StepExecution::Skipped { .. } => CompletionStepOutcome::Skipped,
            StepExecution::Failed { .. } => CompletionStepOutcome::Failed,
        }
    }

    fn detail(&self) -> &str {
        match self {
            StepExecution::Succeeded { detail, .. }
            | StepExecution::Skipped { detail }
            | StepExecution::Failed { detail } => detail,
        }
    }
}

/// The completion-step executor: git (through the workspace's
/// [`WorktreeManager`], never a shell string), egress policy and the PR
/// command supervisor.
pub struct CompletionStepRunner {
    git: WorktreeManager,
    supervisor: Arc<ProcessSupervisor>,
    egress: Arc<dyn EgressPolicy>,
    config: CompletionStepsConfig,
    /// The wired completion-step SCM adapter (`None` = no native PR can be
    /// certified: a contracted PR step records an explicit configuration
    /// blocker; the legacy command path stays available to tests only).
    scm_provider: Option<Arc<dyn CompletionScm>>,
    /// Test-only seam: invoked immediately BEFORE the per-step proof
    /// revalidation, so adversarial tests can inject an edit at the exact
    /// verification/step boundary. Never compiled in production.
    #[cfg(test)]
    pre_step_hook: Option<Arc<dyn Fn(CompletionStep) + Send + Sync>>,
    /// Test-only seam: invoked UNDER the repository mutation guard, after
    /// the proof revalidation of the verified commit and BEFORE the exact
    /// tree is constructed from the manifest (the add/tree/commit window).
    #[cfg(test)]
    commit_tree_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Test-only deterministic crash point of the typed PR operation.
    #[cfg(test)]
    pr_crash: Option<PrOperationCrashPoint>,
}

/// The durable identity context of one PR step execution (the production
/// proof path only): the operator folds it into the external-operation key.
struct PrOperationContext<'a> {
    handle: &'a SessionHandle,
    task_id: TaskId,
    revision: u64,
}

/// The production proof context handed to the commit/push/PR steps: the
/// immutable verification record plus the contract revision, so each step
/// can build/load the ONE [`VerifiedGitArtifact`] and re-assert it
/// immediately before its side effect. `None` = the legacy test-only
/// execution path (no proof, no artifact, no verified publication).
struct ArtifactContext<'a> {
    handle: &'a SessionHandle,
    task_id: TaskId,
    revision: u64,
    proof: VerificationRecordId,
}

impl std::fmt::Debug for CompletionStepRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletionStepRunner")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl CompletionStepRunner {
    /// Build a runner over the daemon's process supervisor. The config is
    /// validated strictly BEFORE any step can run.
    pub fn new(
        supervisor: Arc<ProcessSupervisor>,
        egress: Arc<dyn EgressPolicy>,
        config: CompletionStepsConfig,
    ) -> Result<Self, CompletionStepError> {
        config.validate().map_err(CompletionStepError::Config)?;
        Ok(Self {
            git: WorktreeManager::new(supervisor.clone()),
            supervisor,
            egress,
            config,
            scm_provider: None,
            #[cfg(test)]
            pre_step_hook: None,
            #[cfg(test)]
            commit_tree_hook: None,
            #[cfg(test)]
            pr_crash: None,
        })
    }

    /// Wire the canonical SCM reconciliation adapter of the native PR step
    /// (`faktor_scm::GitHubCompletionScm` over the real GitHub App adapter in
    /// production). Without it a contracted PR step can never certify: it
    /// records the explicit [`NATIVE_PR_SCM_NOT_CONFIGURED`] blocker.
    pub fn with_scm_provider(mut self, provider: Arc<dyn CompletionScm>) -> Self {
        self.scm_provider = Some(provider);
        self
    }

    /// Wire the canonical SCM adapter when one is configured; `None` leaves
    /// the fail-closed configuration blocker in place (the executor's lazy
    /// runner path uses this directly).
    pub fn with_scm_provider_or_none(mut self, provider: Option<Arc<dyn CompletionScm>>) -> Self {
        self.scm_provider = provider;
        self
    }

    /// Test-only: install a hook invoked before every step's proof
    /// revalidation (adversarial race injection).
    #[cfg(test)]
    pub fn with_pre_step_hook(mut self, hook: Arc<dyn Fn(CompletionStep) + Send + Sync>) -> Self {
        self.pre_step_hook = Some(hook);
        self
    }

    /// Test-only: install a hook invoked between the verified commit's proof
    /// revalidation and its exact tree construction.
    #[cfg(test)]
    pub fn with_commit_tree_hook(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.commit_tree_hook = Some(hook);
        self
    }

    /// Test-only: install the deterministic PR-operation crash point.
    #[cfg(test)]
    pub fn with_pr_crash_seam(mut self, crash: PrOperationCrashPoint) -> Self {
        self.pr_crash = Some(crash);
        self
    }

    pub fn config(&self) -> &CompletionStepsConfig {
        &self.config
    }

    /// LEGACY TEST-ONLY authorization path (the P0 residual is closed):
    /// production callers MUST use [`Self::run_completion_steps`] with an
    /// explicit [`VerificationRecordId`] proof — the durable verification
    /// fact alone (`verification_passed`) is never a production
    /// authorization. This shim exists only for the runner's own unit tests,
    /// which call it directly; the executor's production drivers compile
    /// against the proof-validated API exclusively.
    ///
    /// Execute and durably record every requested step of the task's
    /// ACCEPTED contract. With no contract (or the all-false default) this
    /// is a pure no-op: no runner step runs and no durable row is written,
    /// so the default completion path stays byte-identical.
    ///
    /// Idempotent under retry: a step whose latest durable row is already
    /// `Succeeded` is not re-executed and not re-recorded; a terminal
    /// `Failed` row stops everything (nothing new is written); a `Skipped`
    /// row is retried (it is the retryable unmet state).
    #[cfg(test)]
    pub async fn run(
        &self,
        handle: &SessionHandle,
        task_id: TaskId,
        ctx: &CompletionStepContext,
    ) -> Result<CompletionStepReport, CompletionStepError> {
        let Some((revision, contract)) = handle.completion_contract(task_id)? else {
            return Ok(CompletionStepReport::default());
        };
        let requested = contract.requested_steps();
        if requested.is_empty() {
            return Ok(CompletionStepReport::default());
        }
        let rows = handle.ledger_completion_step_statuses(task_id.raw(), revision.raw())?;
        let mut latest: BTreeMap<CompletionStep, (CompletionStepOutcome, String)> = BTreeMap::new();
        for row in rows {
            latest.insert(row.step, (row.status, row.detail));
        }
        // A terminal failure anywhere in the contract revision is final: the
        // gate can never certify under this revision, so nothing is
        // re-executed and nothing new is written (a later Succeeded row
        // could not resurrect it either).
        if latest
            .values()
            .any(|(status, _)| *status == CompletionStepOutcome::Failed)
        {
            let failed_tag = latest
                .iter()
                .find(|(_, (status, _))| *status == CompletionStepOutcome::Failed)
                .map(|(step, (_, _))| step.tag())
                .unwrap_or("a prior step");
            let report = CompletionStepReport {
                records: requested
                    .into_iter()
                    .map(|step| {
                        let (status, detail) = match latest.get(&step) {
                            Some((status, detail)) => (*status, detail.clone()),
                            None => (
                                CompletionStepOutcome::Skipped,
                                format!(
                                    "not attempted: {failed_tag} failed terminally under this contract revision"
                                ),
                            ),
                        };
                        CompletionStepRecord {
                            step,
                            status,
                            detail,
                            seq: None,
                        }
                    })
                    .collect(),
                pr_url: None,
            };
            return Ok(report);
        }
        let mut report = CompletionStepReport::default();
        for step in requested {
            match latest.get(&step) {
                Some((CompletionStepOutcome::Succeeded, detail)) => {
                    report.records.push(CompletionStepRecord {
                        step,
                        status: CompletionStepOutcome::Succeeded,
                        detail: bounded_detail(&format!(
                            "already succeeded (idempotent replay): {detail}"
                        )),
                        seq: None,
                    });
                    continue;
                }
                Some((CompletionStepOutcome::Skipped, _))
                | Some((CompletionStepOutcome::Invalidated, _)) => {
                    // Retryable unmet: fall through and execute again (the
                    // proof revalidation decides whether the side effect may
                    // run at all).
                }
                Some((CompletionStepOutcome::Failed, _)) => unreachable!("pre-scanned above"),
                None => {}
            }
            let execution = self.execute(step, ctx, None, None).await?;
            let detail = bounded_detail(execution.detail());
            let seq = handle
                .set_completion_step_status(task_id, step, execution.status(), &detail)
                .map_err(CompletionStepError::Session)?;
            if let StepExecution::Succeeded {
                pr_url: Some(url), ..
            } = &execution
            {
                report.pr_url = Some(url.clone());
            }
            report.records.push(CompletionStepRecord {
                step,
                status: execution.status(),
                detail,
                seq: Some(seq),
            });
            if execution.status() == CompletionStepOutcome::Failed {
                break;
            }
        }
        // A Failed step stops the ordered execution; every remaining
        // requested step is recorded Skipped ("not attempted") so the
        // durable picture names WHY it is missing.
        if let Some(failed) = report.failed_step() {
            let failed = failed.tag();
            let remaining: Vec<CompletionStep> = contract
                .requested_steps()
                .into_iter()
                .filter(|s| !report.records.iter().any(|r| r.step == *s))
                .collect();
            for step in remaining {
                let detail = bounded_detail(&format!(
                    "not attempted: {failed} failed (terminal for this contract revision)"
                ));
                let seq = handle
                    .set_completion_step_status(
                        task_id,
                        step,
                        CompletionStepOutcome::Skipped,
                        &detail,
                    )
                    .map_err(CompletionStepError::Session)?;
                report.records.push(CompletionStepRecord {
                    step,
                    status: CompletionStepOutcome::Skipped,
                    detail,
                    seq: Some(seq),
                });
            }
        }
        Ok(report)
    }

    /// Execute the requested steps of the task's accepted contract ONLY
    /// while `proof` remains the immutable completion basis of the current
    /// task revision (P0 completion-side-effect gating).
    ///
    /// Before ANY step runs, [`SessionHandle::verify_completion_proof_binding`]
    /// validates the immutable record: task/revision/status/coverage/
    /// workspace/worktree/tree hash, the integration record's final snapshot
    /// equal to the record's tree hash, and the CURRENT candidate root
    /// digesting to that same tree hash. Each side effect revalidates again
    /// IMMEDIATELY before it executes; a moved root records the step (and
    /// every remaining requested step) `Invalidated` — a retryable refusal:
    /// the task stays Verifying and the gate reports the unmet step, never a
    /// terminal failure and never a silent success.
    pub async fn run_completion_steps(
        &self,
        handle: &SessionHandle,
        task_id: TaskId,
        proof: VerificationRecordId,
        ctx: &CompletionStepContext,
    ) -> Result<CompletionStepReport, CompletionStepError> {
        let Some((revision, contract)) = handle.completion_contract(task_id)? else {
            return Ok(CompletionStepReport::default());
        };
        let requested = contract.requested_steps();
        if requested.is_empty() {
            return Ok(CompletionStepReport::default());
        }
        // ---- gate 1: validate the immutable proof BEFORE anything runs ----
        if let Err(err) =
            handle.verify_completion_proof_binding(task_id, proof, Some(ctx.root.as_path()))
        {
            let detail = format!(
                "completion proof {proof} is not the current immutable basis of task {task_id}: {err}"
            );
            return self
                .record_invalidated(handle, task_id, &requested, &detail)
                .map(|records| CompletionStepReport {
                    records,
                    pr_url: None,
                });
        }
        let rows = handle.ledger_completion_step_statuses(task_id.raw(), revision.raw())?;
        let mut latest: BTreeMap<CompletionStep, (CompletionStepOutcome, String)> = BTreeMap::new();
        for row in rows {
            latest.insert(row.step, (row.status, row.detail));
        }
        if latest
            .values()
            .any(|(status, _)| *status == CompletionStepOutcome::Failed)
        {
            return self.terminal_report(&requested, &latest);
        }
        let mut report = CompletionStepReport::default();
        for step in requested {
            #[cfg(test)]
            if let Some(hook) = &self.pre_step_hook {
                hook(step);
            }
            // ---- gate 2: revalidate IMMEDIATELY before this side effect ----
            if let Err(err) =
                handle.verify_completion_proof_binding(task_id, proof, Some(ctx.root.as_path()))
            {
                let detail = format!("verified root invalidated before step {step:?}: {err}");
                let remaining: Vec<CompletionStep> = contract
                    .requested_steps()
                    .into_iter()
                    .filter(|s| !report.records.iter().any(|r| r.step == *s))
                    .collect();
                let records = self.record_invalidated(handle, task_id, &remaining, &detail)?;
                report.records.extend(records);
                return Ok(report);
            }
            match latest.get(&step) {
                Some((CompletionStepOutcome::Succeeded, detail)) => {
                    report.records.push(CompletionStepRecord {
                        step,
                        status: CompletionStepOutcome::Succeeded,
                        detail: bounded_detail(&format!(
                            "already succeeded (idempotent replay): {detail}"
                        )),
                        seq: None,
                    });
                    continue;
                }
                Some((CompletionStepOutcome::Skipped, _))
                | Some((CompletionStepOutcome::Invalidated, _)) => {}
                Some((CompletionStepOutcome::Failed, _)) => unreachable!("pre-scanned above"),
                None => {}
            }
            let pr_op = PrOperationContext {
                handle,
                task_id,
                revision: revision.raw(),
            };
            let artifact_op = ArtifactContext {
                handle,
                task_id,
                revision: revision.raw(),
                proof,
            };
            let execution = self
                .execute(step, ctx, Some(&pr_op), Some(&artifact_op))
                .await?;
            let detail = bounded_detail(execution.detail());
            let seq = handle
                .set_completion_step_status(task_id, step, execution.status(), &detail)
                .map_err(CompletionStepError::Session)?;
            if let StepExecution::Succeeded {
                pr_url: Some(url), ..
            } = &execution
            {
                report.pr_url = Some(url.clone());
            }
            report.records.push(CompletionStepRecord {
                step,
                status: execution.status(),
                detail,
                seq: Some(seq),
            });
            if execution.status() == CompletionStepOutcome::Failed {
                break;
            }
        }
        if let Some(failed) = report.failed_step() {
            let failed = failed.tag();
            let remaining: Vec<CompletionStep> = contract
                .requested_steps()
                .into_iter()
                .filter(|s| !report.records.iter().any(|r| r.step == *s))
                .collect();
            for step in remaining {
                let detail = bounded_detail(&format!(
                    "not attempted: {failed} failed (terminal for this contract revision)"
                ));
                let seq = handle
                    .set_completion_step_status(
                        task_id,
                        step,
                        CompletionStepOutcome::Skipped,
                        &detail,
                    )
                    .map_err(CompletionStepError::Session)?;
                report.records.push(CompletionStepRecord {
                    step,
                    status: CompletionStepOutcome::Skipped,
                    detail,
                    seq: Some(seq),
                });
            }
        }
        Ok(report)
    }

    /// Record every named step `Invalidated` with the shared reason and
    /// return the records in gate order.
    fn record_invalidated(
        &self,
        handle: &SessionHandle,
        task_id: TaskId,
        steps: &[CompletionStep],
        reason: &str,
    ) -> Result<Vec<CompletionStepRecord>, CompletionStepError> {
        let mut records = Vec::with_capacity(steps.len());
        for step in steps {
            let detail = bounded_detail(reason);
            let seq = handle
                .set_completion_step_status(
                    task_id,
                    *step,
                    CompletionStepOutcome::Invalidated,
                    &detail,
                )
                .map_err(CompletionStepError::Session)?;
            records.push(CompletionStepRecord {
                step: *step,
                status: CompletionStepOutcome::Invalidated,
                detail,
                seq: Some(seq),
            });
        }
        Ok(records)
    }

    /// The deterministic report of a contract revision already carrying a
    /// terminal `Failed` row: nothing is re-executed and nothing new is
    /// written.
    fn terminal_report(
        &self,
        requested: &[CompletionStep],
        latest: &BTreeMap<CompletionStep, (CompletionStepOutcome, String)>,
    ) -> Result<CompletionStepReport, CompletionStepError> {
        let failed_tag = latest
            .iter()
            .find(|(_, (status, _))| *status == CompletionStepOutcome::Failed)
            .map(|(step, (_, _))| step.tag())
            .unwrap_or("a prior step");
        Ok(CompletionStepReport {
            records: requested
                .iter()
                .map(|step| {
                    let (status, detail) = match latest.get(step) {
                        Some((status, detail)) => (*status, detail.clone()),
                        None => (
                            CompletionStepOutcome::Skipped,
                            format!(
                                "not attempted: {failed_tag} failed terminally under this contract revision"
                            ),
                        ),
                    };
                    CompletionStepRecord {
                        step: *step,
                        status,
                        detail,
                        seq: None,
                    }
                })
                .collect(),
            pr_url: None,
        })
    }

    async fn execute(
        &self,
        step: CompletionStep,
        ctx: &CompletionStepContext,
        pr: Option<&PrOperationContext<'_>>,
        artifact: Option<&ArtifactContext<'_>>,
    ) -> Result<StepExecution, CompletionStepError> {
        match step {
            CompletionStep::Commit => Ok(self.execute_commit(ctx, artifact).await),
            CompletionStep::Push => Ok(self.execute_push(ctx, artifact).await),
            CompletionStep::Pr => self.execute_pr(ctx, pr, artifact).await,
        }
    }

    async fn execute_commit(
        &self,
        ctx: &CompletionStepContext,
        artifact: Option<&ArtifactContext<'_>>,
    ) -> StepExecution {
        let message = commit_message(&ctx.goal);
        if let Some(artifact_ctx) = artifact {
            return self
                .execute_verified_commit(ctx, artifact_ctx, &message)
                .await;
        }
        // Legacy test-only path (no immutable proof): the historic
        // `commit_all`. Production never reaches it.
        match self
            .git
            .commit_all(&ctx.root, &message, ProcessOwner::Daemon)
            .await
        {
            Ok(CommitOutcome::Committed { sha, subject }) => StepExecution::Succeeded {
                detail: format!(
                    "committed {} on the current branch: {subject}",
                    short_sha(&sha)
                ),
                pr_url: None,
            },
            Ok(CommitOutcome::NothingToCommit) => {
                // Crash between the commit and its status row: HEAD already
                // carries the SAME deterministic message, so the step IS
                // done — an idempotent success, never a poisoning Skipped.
                match self.git.head_message(&ctx.root, ProcessOwner::Daemon).await {
                    Ok(Some(previous)) if previous.trim() == message => StepExecution::Succeeded {
                        detail: "already committed (idempotent replay: HEAD carries the \
                                 deterministic completion message)"
                            .into(),
                        pr_url: None,
                    },
                    _ => StepExecution::Skipped {
                        detail: "nothing to commit (working tree clean)".into(),
                    },
                }
            }
            Ok(CommitOutcome::EmptyRepository) => StepExecution::Skipped {
                detail: "empty repository state (unborn HEAD); refusing an initial commit".into(),
            },
            Err(e) => StepExecution::Failed {
                detail: format!("git commit failed: {}", e.message),
            },
        }
    }

    /// Build (or reload) the ONE durable publication artifact of this
    /// contract revision: the canonical manifest of the verified root, whose
    /// digest must equal the immutable verification record's tree hash. A
    /// recorded artifact whose manifest has drifted from the record is a
    /// typed refusal (the artifact is authoritative once written).
    fn load_or_build_artifact(
        &self,
        actx: &ArtifactContext<'_>,
        root: &std::path::Path,
    ) -> Result<VerifiedGitArtifact, String> {
        let record = actx
            .handle
            .get_verification_record(actx.proof)
            .map_err(|e| format!("verified-git artifact record read: {e}"))?
            .ok_or_else(|| {
                format!(
                    "verified-git artifact: verification record {} does not exist",
                    actx.proof
                )
            })?;
        if record.task_id.raw() != actx.task_id.raw() {
            return Err(format!(
                "verified-git artifact: record {} belongs to task {}, not task {}",
                actx.proof,
                record.task_id.raw(),
                actx.task_id.raw()
            ));
        }
        let digest = crate::runtime::task_executor::root_manifest_digest(root)
            .map_err(|e| format!("verified-git artifact root digest: {e}"))?;
        // The record's tree hash is the immutable anchor when present. A
        // legacy record without one still binds through the proof protocol;
        // the artifact then self-anchors on the digest taken here, and every
        // later step re-asserts THAT digest.
        if let Some(record_tree) = record.tree_hash.as_deref() {
            if digest != record_tree {
                return Err(format!(
                    "verified-git artifact: the root digests to {digest} but the verification record certifies {record_tree}"
                ));
            }
        }
        let rows = crate::runtime::merge::canonical_manifest_rows(root, MAX_BASE_ENTRIES)
            .map_err(|e| format!("verified-git artifact manifest: {e}"))?;
        let manifest: Vec<VerifiedManifestEntry> = rows
            .into_iter()
            .map(|(path, state)| VerifiedManifestEntry {
                path: path.to_string_lossy().into_owned(),
                state,
            })
            .collect();
        let existing = actx
            .handle
            .ledger_verified_git_artifact_get(actx.task_id.raw(), actx.revision)
            .map_err(|e| format!("verified-git artifact ledger read: {e}"))?;
        if let Some(previous) = existing {
            if previous.verification_record != actx.proof.raw() {
                return Err(format!(
                    "verified-git artifact already certifies record {} but this step runs for record {}",
                    previous.verification_record, actx.proof
                ));
            }
            if previous.verified_root_digest != digest || previous.verified_manifest != manifest {
                return Err(format!(
                    "verified-git artifact manifest has drifted from the recorded artifact of task {}/revision {}",
                    actx.task_id.raw(),
                    actx.revision
                ));
            }
            return Ok(previous);
        }
        Ok(VerifiedGitArtifact {
            task_id: actx.task_id.raw(),
            revision: actx.revision,
            verification_record: actx.proof.raw(),
            verified_root_digest: digest,
            verified_manifest: manifest,
            git_tree_oid: None,
            commit_oid: None,
            local_ref: None,
            remote_ref: None,
            updated_ms: actx.handle.now_ms(),
        })
    }

    fn persist_artifact(
        &self,
        actx: &ArtifactContext<'_>,
        artifact: &VerifiedGitArtifact,
    ) -> Result<(), String> {
        actx.handle
            .ledger_verified_git_artifact_set(artifact)
            .map(|_| ())
            .map_err(|e| format!("verified-git artifact write: {e}"))
    }

    /// The PRODUCTION commit step: ONE repository mutation guard is held
    /// across proof revalidation, the exact tree construction from the
    /// verified manifest (asserted equal to it), `commit-tree` with that
    /// tree + parent HEAD, the branch-ref update and the HEAD read-back; the
    /// commit OID is durably recorded in the artifact. A worktree change
    /// injected after revalidation is a typed conflict with nothing
    /// committed; no `git add` ever touches an unverified checkout.
    async fn execute_verified_commit(
        &self,
        ctx: &CompletionStepContext,
        actx: &ArtifactContext<'_>,
        message: &str,
    ) -> StepExecution {
        let owner = ProcessOwner::Daemon;
        let root = &ctx.root;
        let mut artifact = match self.load_or_build_artifact(actx, root) {
            Ok(artifact) => artifact,
            Err(detail) => return StepExecution::Failed { detail },
        };
        let guard = match self.git.acquire_mutation_guard(root, owner.clone()).await {
            Ok(guard) => guard,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("repository mutation guard: {}", e.message),
                };
            }
        };
        // Revalidate the immutable proof IMMEDIATELY before the commit, with
        // the repository mutation guard held.
        if let Err(e) = actx.handle.verify_completion_proof_binding(
            actx.task_id,
            actx.proof,
            Some(root.as_path()),
        ) {
            return StepExecution::Failed {
                detail: format!("verified root invalidated immediately before the commit: {e}"),
            };
        }
        #[cfg(test)]
        if let Some(hook) = &self.commit_tree_hook {
            hook();
        }
        let head = match self.git.head_sha_unlocked(root, owner.clone()).await {
            Ok(head) => head,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git head read failed: {}", e.message),
                };
            }
        };
        let head_tree = match &head {
            Some(_) => match self
                .git
                .git_unlocked(root, &["rev-parse", "HEAD^{tree}"], owner.clone())
                .await
            {
                Ok(tree) => Some(tree.trim().to_string()),
                Err(e) => {
                    return StepExecution::Failed {
                        detail: format!("git HEAD tree read failed: {}", e.message),
                    };
                }
            },
            None => None,
        };
        // Idempotent replay: the artifact already names the live commit.
        if let Some(commit) = artifact.commit_oid.clone() {
            if head.as_deref() == Some(commit.as_str()) {
                if let Some(tree) = artifact.git_tree_oid.clone() {
                    if head_tree.as_deref() != Some(tree.as_str()) {
                        return StepExecution::Failed {
                            detail: format!(
                                "the recorded verified commit {commit} is HEAD but its tree is {:?}, not {tree}",
                                head_tree
                            ),
                        };
                    }
                }
                return StepExecution::Succeeded {
                    detail: format!(
                        "already committed the verified tree at {} (idempotent replay)",
                        short_sha(&commit)
                    ),
                    pr_url: None,
                };
            }
            return StepExecution::Failed {
                detail: format!(
                    "the recorded verified commit {commit} is not HEAD ({:?}); the repository moved after publication",
                    head
                ),
            };
        }
        let branch = match self.git.current_branch_unlocked(root, owner.clone()).await {
            Ok(branch) if !branch.is_empty() => branch,
            Ok(_) => {
                return StepExecution::Failed {
                    detail: "cannot commit: the repository is on a detached HEAD".into(),
                };
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                };
            }
        };
        if head.is_none() {
            return StepExecution::Skipped {
                detail: "empty repository state (unborn HEAD); refusing an initial commit".into(),
            };
        }
        let entries: Vec<VerifiedTreeEntry> = artifact
            .verified_manifest
            .iter()
            .map(|entry| VerifiedTreeEntry {
                path: entry.path.clone(),
                state: entry.state.clone(),
            })
            .collect();
        let tree = match self
            .git
            .build_tree_from_manifest(&guard, root, root, &entries, owner.clone())
            .await
        {
            Ok(tree) => tree,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("verified tree construction refused: {}", e.message),
                };
            }
        };
        let local_ref = format!("refs/heads/{branch}");
        if head_tree.as_deref() == Some(tree.as_str()) {
            // HEAD already carries the verified tree: record the identity,
            // mint no redundant commit.
            artifact.git_tree_oid = Some(tree);
            artifact.commit_oid = head.clone();
            artifact.local_ref = Some(local_ref);
            artifact.updated_ms = actx.handle.now_ms();
            if let Err(detail) = self.persist_artifact(actx, &artifact) {
                return StepExecution::Failed { detail };
            }
            // Keep git's own index in step with HEAD (a normal commit does).
            if let Err(e) = self
                .git
                .git_unlocked(root, &["read-tree", "HEAD"], owner.clone())
                .await
            {
                return StepExecution::Failed {
                    detail: format!(
                        "index refresh after the verified commit failed: {}",
                        e.message
                    ),
                };
            }
            return StepExecution::Succeeded {
                detail: format!(
                    "already committed: HEAD carries the verified tree ({})",
                    head.as_deref().map(short_sha).unwrap_or_default()
                ),
                pr_url: None,
            };
        }
        match self
            .git
            .commit_verified_tree(
                &guard,
                root,
                root,
                &branch,
                message,
                &entries,
                owner.clone(),
            )
            .await
        {
            Ok(CommitOutcome::Committed { sha, .. }) => {
                artifact.git_tree_oid = Some(tree);
                artifact.commit_oid = Some(sha.clone());
                artifact.local_ref = Some(local_ref);
                artifact.updated_ms = actx.handle.now_ms();
                if let Err(detail) = self.persist_artifact(actx, &artifact) {
                    return StepExecution::Failed { detail };
                }
                StepExecution::Succeeded {
                    detail: format!(
                        "committed the verified tree at {} on {branch}: {message}",
                        short_sha(&sha)
                    ),
                    pr_url: None,
                }
            }
            Ok(CommitOutcome::NothingToCommit) => StepExecution::Failed {
                detail: "the verified commit reported nothing to commit although HEAD's tree                          differs from the verified manifest"
                    .into(),
            },
            Ok(CommitOutcome::EmptyRepository) => StepExecution::Skipped {
                detail: "empty repository state (unborn HEAD); refusing an initial commit".into(),
            },
            Err(e) => StepExecution::Failed {
                detail: format!("verified commit failed: {}", e.message),
            },
        }
    }

    /// The PRODUCTION push step: the exact durable commit OID is pushed to
    /// its branch ref (lease against the observed remote state), the live
    /// remote ref is read back and MUST be the exact OID, and the encoded
    /// remote ref is recorded in the artifact for the PR step to re-assert.
    async fn execute_verified_push(
        &self,
        ctx: &CompletionStepContext,
        actx: &ArtifactContext<'_>,
    ) -> StepExecution {
        let owner = ProcessOwner::Daemon;
        let root = &ctx.root;
        let mut artifact = match self.load_or_build_artifact(actx, root) {
            Ok(artifact) => artifact,
            Err(detail) => return StepExecution::Failed { detail },
        };
        let branch = match artifact
            .local_ref
            .as_deref()
            .and_then(|r| r.strip_prefix("refs/heads/"))
            .map(str::to_string)
        {
            Some(branch) if !branch.is_empty() => branch,
            _ => match self.git.current_branch(root, owner.clone()).await {
                Ok(branch) if !branch.is_empty() => branch,
                _ => {
                    return StepExecution::Failed {
                        detail: "cannot push: no recorded local ref and detached HEAD".into(),
                    };
                }
            },
        };
        let remote = self.config.remote.clone();
        let url = match self.git.remote_url(root, &remote, owner.clone()).await {
            Ok(None) => {
                return StepExecution::Skipped {
                    detail: "no git remote configured (the repository has no remotes)".into(),
                };
            }
            Ok(Some(url)) => url,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                };
            }
        };
        if is_egress_destination(&url) {
            if let Err(denied) = self.egress.check(&url) {
                return StepExecution::Failed {
                    detail: format!("egress policy denied push to {url}: {denied}"),
                };
            }
        }
        let guard = match self.git.acquire_mutation_guard(root, owner.clone()).await {
            Ok(guard) => guard,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("repository mutation guard: {}", e.message),
                };
            }
        };
        // Revalidate the proof and the recorded OIDs immediately before the
        // push: the live HEAD must still be exactly the verified commit.
        if let Err(e) = actx.handle.verify_completion_proof_binding(
            actx.task_id,
            actx.proof,
            Some(root.as_path()),
        ) {
            return StepExecution::Failed {
                detail: format!("verified root invalidated immediately before the push: {e}"),
            };
        }
        // Resolve the EXACT commit to publish. A push-only contract (or a
        // crash before the commit step recorded its OID) publishes the live
        // HEAD ONLY when HEAD's tree is the verified manifest tree — built
        // here from the manifest and asserted, never assumed.
        let commit = match artifact.commit_oid.clone() {
            Some(commit) => {
                match self.git.head_sha_unlocked(root, owner.clone()).await {
                    Ok(Some(head)) if head == commit => {}
                    Ok(other) => {
                        return StepExecution::Failed {
                            detail: format!(
                                "the recorded verified commit {commit} is not the live HEAD {other:?}; refusing to publish a moved repository"
                            ),
                        };
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git head read failed: {}", e.message),
                        };
                    }
                }
                commit
            }
            None => {
                let head = match self.git.head_sha_unlocked(root, owner.clone()).await {
                    Ok(Some(head)) => head,
                    Ok(None) => {
                        return StepExecution::Failed {
                            detail: "cannot push: HEAD is unborn".into(),
                        };
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git head read failed: {}", e.message),
                        };
                    }
                };
                let entries: Vec<VerifiedTreeEntry> = artifact
                    .verified_manifest
                    .iter()
                    .map(|entry| VerifiedTreeEntry {
                        path: entry.path.clone(),
                        state: entry.state.clone(),
                    })
                    .collect();
                let tree = match self
                    .git
                    .build_tree_from_manifest(&guard, root, root, &entries, owner.clone())
                    .await
                {
                    Ok(tree) => tree,
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!(
                                "verified tree construction refused before the push: {}",
                                e.message
                            ),
                        };
                    }
                };
                match self
                    .git
                    .git_unlocked(root, &["rev-parse", "HEAD^{tree}"], owner.clone())
                    .await
                {
                    Ok(head_tree) if head_tree.trim() == tree => {}
                    Ok(head_tree) => {
                        return StepExecution::Failed {
                            detail: format!(
                                "cannot publish HEAD {}: its tree {} is not the verified manifest tree {tree}",
                                head,
                                head_tree.trim()
                            ),
                        };
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git HEAD tree read failed: {}", e.message),
                        };
                    }
                }
                artifact.git_tree_oid = Some(tree);
                artifact.commit_oid = Some(head.clone());
                artifact.local_ref = Some(format!("refs/heads/{branch}"));
                artifact.updated_ms = actx.handle.now_ms();
                if let Err(detail) = self.persist_artifact(actx, &artifact) {
                    return StepExecution::Failed { detail };
                }
                head
            }
        };
        // Already published? Read the LIVE remote ref and reconcile it to the
        // exact OID before deciding.
        let live = match self
            .git
            .remote_branch_oid_unlocked(root, &remote, &branch, owner.clone())
            .await
        {
            Ok(oid) => oid,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("remote ref read failed: {}", e.message),
                };
            }
        };
        let (outcome, remote_oid) = if live.as_deref() == Some(commit.as_str()) {
            (
                PushOutcome::AlreadyCurrent {
                    note: "remote already carries the verified commit".into(),
                },
                commit.clone(),
            )
        } else {
            match self
                .git
                .push_exact_commit(&guard, root, &remote, &branch, &commit, None, owner.clone())
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    return StepExecution::Failed {
                        detail: format!("git push failed: {}", e.message),
                    };
                }
            }
        };
        artifact.remote_ref = Some(format!("{remote}:refs/heads/{branch}@{remote_oid}"));
        artifact.updated_ms = actx.handle.now_ms();
        if let Err(detail) = self.persist_artifact(actx, &artifact) {
            return StepExecution::Failed { detail };
        }
        match outcome {
            PushOutcome::Pushed { note } => StepExecution::Succeeded {
                detail: format!(
                    "pushed the verified commit {} to {remote} ({url}){}",
                    short_sha(&commit),
                    note_suffix(&note)
                ),
                pr_url: None,
            },
            PushOutcome::AlreadyCurrent { note } => StepExecution::Succeeded {
                detail: format!(
                    "already pushed the verified commit {} to {remote} ({url}); idempotent replay{}",
                    short_sha(&commit),
                    note_suffix(&note)
                ),
                pr_url: None,
            },
        }
    }

    async fn execute_push(
        &self,
        ctx: &CompletionStepContext,
        artifact: Option<&ArtifactContext<'_>>,
    ) -> StepExecution {
        if let Some(actx) = artifact {
            return self.execute_verified_push(ctx, actx).await;
        }
        let owner = ProcessOwner::Daemon;
        let branch = match self.git.current_branch(&ctx.root, owner.clone()).await {
            Ok(branch) if !branch.is_empty() => branch,
            Ok(_) => {
                return StepExecution::Failed {
                    detail: "cannot push: the repository is on a detached HEAD".into(),
                };
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                };
            }
        };
        let remote = self.config.remote.clone();
        let url = match self.git.remote_url(&ctx.root, &remote, owner.clone()).await {
            Ok(None) => {
                return StepExecution::Skipped {
                    detail: "no git remote configured (the repository has no remotes)".into(),
                };
            }
            Ok(Some(url)) => url,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                };
            }
        };
        // Push is an external effect: consult the egress policy for real
        // network destinations (local path / file:// remotes are not egress).
        if is_egress_destination(&url) {
            if let Err(denied) = self.egress.check(&url) {
                return StepExecution::Failed {
                    detail: format!("egress policy denied push to {url}: {denied}"),
                };
            }
        }
        let head = match self.git.head_sha(&ctx.root, owner.clone()).await {
            Ok(Some(head)) => head,
            Ok(None) => {
                return StepExecution::Failed {
                    detail: "cannot push: HEAD is unborn".into(),
                };
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git head read failed: {}", e.message),
                };
            }
        };
        // Idempotency WITHOUT a network round-trip: a local remote-tracking
        // ref already naming HEAD means an earlier run pushed this commit.
        if let Ok(Some(pushed)) = self
            .git
            .pushed_ref_sha(&ctx.root, &remote, &branch, owner.clone())
            .await
        {
            if pushed == head {
                return StepExecution::Succeeded {
                    detail: format!(
                        "already pushed {branch} to {remote} ({url}); idempotent replay"
                    ),
                    pr_url: None,
                };
            }
        }
        match self
            .git
            .push_branch(&ctx.root, &remote, &branch, owner)
            .await
        {
            Ok(PushOutcome::Pushed { note }) => StepExecution::Succeeded {
                detail: format!("pushed {branch} to {remote} ({url}){}", note_suffix(&note)),
                pr_url: None,
            },
            Ok(PushOutcome::AlreadyCurrent { note }) => StepExecution::Succeeded {
                detail: format!(
                    "already pushed {branch} to {remote} ({url}); up-to-date{}",
                    note_suffix(&note)
                ),
                pr_url: None,
            },
            Err(e) => StepExecution::Failed {
                detail: format!("git push failed: {}", e.message),
            },
        }
    }

    /// The expert-command PR path: executes the configured
    /// `pr_command`/`pr_program` through the supervisor. It records NO
    /// durable external-operation identity and therefore NEVER certifies the
    /// native PR step — production invocations of the proof path refuse the
    /// step before this code runs ([`PR_REQUIRES_TYPED_RECONCILIATION`]);
    /// only the legacy test path reaches it.
    async fn execute_pr_command(&self, ctx: &CompletionStepContext) -> StepExecution {
        if self.config.pr_command.is_none() && self.config.pr_program.is_none() {
            return StepExecution::Skipped {
                detail: "[completion] pr_command is not configured; no PR was created".into(),
            };
        }
        let owner = ProcessOwner::Daemon;
        let branch = match self.git.current_branch(&ctx.root, owner).await {
            Ok(branch) if !branch.is_empty() => branch,
            Ok(_) => {
                return StepExecution::Failed {
                    detail: "cannot create a PR from a detached HEAD (no current branch)".into(),
                };
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                };
            }
        };
        let (program, args) = if let Some(program) = self.config.pr_program.as_deref() {
            match render_pr_argv(
                program,
                &self.config.pr_args,
                &branch,
                &self.config.base_branch,
                &self.config.remote,
            ) {
                Ok(rendered) => rendered,
                Err(e) => {
                    return StepExecution::Failed {
                        detail: format!("pr_program render failed: {e}"),
                    };
                }
            }
        } else {
            let template = self.config.pr_command.as_deref().unwrap_or_default();
            match render_pr_command(
                template,
                &branch,
                &self.config.base_branch,
                &self.config.remote,
            ) {
                Ok(rendered) => rendered,
                Err(e) => {
                    return StepExecution::Failed {
                        detail: format!("pr_command render failed: {e}"),
                    };
                }
            }
        };
        let cfg = SpawnConfig {
            cmd: program,
            args,
            cwd: ctx.root.clone(),
            // The supervisor ALWAYS env_clears and layers this safe baseline
            // (PATH/HOME/platform bits; secret-shaped names are denied) —
            // never the daemon's full environment.
            env: EnvSpec::default_baseline(),
            owner: ProcessOwner::Daemon,
            capture: true,
            ..Default::default()
        };
        let out = match self
            .supervisor
            .run(cfg, PR_COMMAND_TIMEOUT, CancellationToken::new())
            .await
        {
            Ok(out) => out,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("pr command could not run: {}", e.message),
                };
            }
        };
        let excerpt = truncate_bytes(&out.excerpt, MAX_PR_OUTPUT_BYTES);
        let url = parse_pr_url(&out.excerpt);
        let already_exists = out.excerpt.to_ascii_lowercase().contains("already exists");
        match (out.exit_code, already_exists, url) {
            (Some(0), _, Some(url)) => StepExecution::Succeeded {
                detail: format!("pr created: {url}"),
                pr_url: Some(url),
            },
            (Some(0), _, None) => StepExecution::Succeeded {
                detail: format!("pr command exited 0 (no URL in output): {excerpt}"),
                pr_url: None,
            },
            (_, true, url) => StepExecution::Succeeded {
                detail: match &url {
                    Some(url) => format!("pr already exists: {url}"),
                    None => "pr already exists (idempotent replay)".into(),
                },
                pr_url: url,
            },
            (code, _, _) => StepExecution::Failed {
                detail: format!(
                    "pr command exited {}: {excerpt}",
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into())
                ),
            },
        }
    }

    /// The PR step dispatcher. The PROOF path (`pr = Some`) certifies the
    /// native step ONLY through the typed [`ScmProvider`] reconciliation
    /// protocol; the legacy test path (`pr = None`) keeps the expert-command
    /// behavior byte-identical.
    async fn execute_pr(
        &self,
        ctx: &CompletionStepContext,
        pr: Option<&PrOperationContext<'_>>,
        artifact: Option<&ArtifactContext<'_>>,
    ) -> Result<StepExecution, CompletionStepError> {
        match pr {
            None => Ok(self.execute_pr_command(ctx).await),
            Some(op) => self.execute_pr_native(ctx, op, artifact).await,
        }
    }

    /// The typed native PR step: journal the exact operation identity BEFORE
    /// the remote call, reconcile from the durable rows after it, and refuse
    /// typed on any identity drift. The step certifies only when the remote
    /// object id + version were journaled.
    async fn execute_pr_native(
        &self,
        ctx: &CompletionStepContext,
        op: &PrOperationContext<'_>,
        artifact: Option<&ArtifactContext<'_>>,
    ) -> Result<StepExecution, CompletionStepError> {
        let Some(provider) = self.scm_provider.clone() else {
            // Fail-closed: without the canonical SCM adapter nothing can
            // certify the native PR step. A configured generic command is a
            // typed refusal naming its missing protocol; an otherwise
            // unconfigured PR is an EXPLICIT configuration blocker — never
            // the historic silent `Skipped`, so a contracted PR step can
            // never be rebuilt without a provider.
            if self.config.pr_command.is_some() || self.config.pr_program.is_some() {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "native PR refused ({PR_REQUIRES_TYPED_RECONCILIATION}): the configured \
                         pr_command/pr_program is an expert extension with no typed reconciliation \
                         protocol, so it can never certify the native PR step"
                    ),
                });
            }
            return Ok(StepExecution::Failed {
                detail: format!(
                    "native PR refused ({NATIVE_PR_SCM_NOT_CONFIGURED}): the completion contract \
                     requests a pull request but no SCM provider is configured; wire the GitHub \
                     App ([cloud.github_app]) with installation credentials and a completed \
                     repository sync, or drop include_pr"
                ),
            });
        };
        let owner = ProcessOwner::Daemon;
        let branch = match self.git.current_branch(&ctx.root, owner.clone()).await {
            Ok(branch) if !branch.is_empty() => branch,
            Ok(_) => {
                return Ok(StepExecution::Failed {
                    detail: "cannot certify the native PR step from a detached HEAD".into(),
                });
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                });
            }
        };
        let remote_url = match self
            .git
            .remote_url(&ctx.root, &self.config.remote, owner.clone())
            .await
        {
            Ok(Some(url)) => url,
            Ok(None) => {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "cannot certify the native PR step: git remote {} is not configured",
                        self.config.remote
                    ),
                });
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                });
            }
        };
        // Wave-0 exact binding: when the verified-git artifact recorded a
        // remote ref, the live local HEAD and the LIVE remote head must both
        // still be the exact verified commit before any PR is created or
        // updated. A moved remote is a typed refusal, never a PR on top of a
        // different commit.
        if let Some(actx) = artifact {
            let recorded = actx
                .handle
                .ledger_verified_git_artifact_get(actx.task_id.raw(), actx.revision)
                .map_err(|e| CompletionStepError::Config(format!("artifact read: {e}")))?;
            if let Some(recorded) = recorded {
                if let (Some(commit), Some(remote_ref)) = (
                    recorded.commit_oid.as_deref(),
                    recorded.remote_ref.as_deref(),
                ) {
                    let live_head = match self.git.head_sha(&ctx.root, ProcessOwner::Daemon).await {
                        Ok(Some(head)) => head,
                        Ok(None) => {
                            return Ok(StepExecution::Failed {
                                detail: "native PR refused: HEAD is unborn".into(),
                            });
                        }
                        Err(e) => {
                            return Ok(StepExecution::Failed {
                                detail: format!(
                                    "native PR refused: git head read failed: {}",
                                    e.message
                                ),
                            });
                        }
                    };
                    if commit != live_head {
                        return Ok(StepExecution::Failed {
                            detail: format!(
                                "native PR refused: the artifact records commit {commit} but the live HEAD is {live_head}"
                            ),
                        });
                    }
                    let Some((remote_name, rest)) = remote_ref.split_once(':') else {
                        return Ok(StepExecution::Failed {
                            detail: format!(
                                "native PR refused: the recorded remote ref {remote_ref:?} is malformed"
                            ),
                        });
                    };
                    let Some((refname, recorded_oid)) = rest.rsplit_once('@') else {
                        return Ok(StepExecution::Failed {
                            detail: format!(
                                "native PR refused: the recorded remote ref {remote_ref:?} is malformed"
                            ),
                        });
                    };
                    let branch_name = refname
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch.as_str());
                    let live = self
                        .git
                        .remote_branch_oid(
                            &ctx.root,
                            remote_name,
                            branch_name,
                            ProcessOwner::Daemon,
                        )
                        .await
                        .map_err(|e| {
                            CompletionStepError::Config(format!("remote ref read: {e}"))
                        })?;
                    if recorded_oid != commit || live.as_deref() != Some(commit) {
                        return Ok(StepExecution::Failed {
                            detail: format!(
                                "native PR refused: the recorded remote head {recorded_oid} on {refname}                                  does not match the live remote head {live:?} for commit {commit}"
                            ),
                        });
                    }
                }
            }
        }
        let Some((organization, repository)) = parse_scm_repository(&remote_url) else {
            return Ok(StepExecution::Failed {
                detail: format!(
                    "cannot certify the native PR step: remote {remote_url} is not a recognizable \
                     GitHub repository reference (organization/repository)"
                ),
            });
        };
        let head_sha = match self.git.head_sha(&ctx.root, owner).await {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                return Ok(StepExecution::Failed {
                    detail: "cannot certify the native PR step: HEAD is unborn".into(),
                });
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git head read failed: {}", e.message),
                });
            }
        };
        let marker = native_pr_marker(op.task_id, op.revision);
        let input = ExternalOperationInput {
            organization: organization.clone(),
            repository: repository.clone(),
            head: branch.clone(),
            base: self.config.base_branch.clone(),
            marker: marker.clone(),
        };
        let provider_name = provider.provider_name();
        let operation_key = native_pr_operation_key(op.task_id, op.revision, provider_name);
        // Read the durable identity BEFORE any remote call: a conflicting
        // input identity or a recorded terminal failure refuses WITHOUT
        // touching the provider (no branch reconciliation, no PR lookup).
        let recorded = match op.handle.ledger_external_operation_read(&operation_key) {
            DurableRead::Missing => None,
            DurableRead::PresentValid(row) => Some(row),
            DurableRead::PresentMalformed(detail) | DurableRead::StoreFailure(detail) => {
                let err = ExternalOperationError::Store {
                    operation_key: operation_key.clone(),
                    detail,
                };
                return Ok(step_refusal(&err));
            }
        };
        if let Some(row) = &recorded {
            if row.input != input {
                let err = ExternalOperationError::InputIdentityConflict {
                    operation_key: operation_key.clone(),
                    detail: format!(
                        "recorded (org {org_r}, repo {repo_r}, head {head_r}, base {base_r}, \
                         marker {marker_r}) vs current (org {org_c}, repo {repo_c}, head {head_c}, \
                         base {base_c}, marker {marker_c}); the same operation key never silently \
                         retargets",
                        org_r = row.input.organization,
                        repo_r = row.input.repository,
                        head_r = row.input.head,
                        base_r = row.input.base,
                        marker_r = row.input.marker,
                        org_c = input.organization,
                        repo_c = input.repository,
                        head_c = input.head,
                        base_c = input.base,
                        marker_c = input.marker,
                    ),
                };
                return Ok(step_refusal(&err));
            }
            if row.state == ExternalOperationState::Failed {
                let err = ExternalOperationError::RecordedFailure {
                    operation_key: operation_key.clone(),
                    detail: row
                        .remote_object_version
                        .clone()
                        .unwrap_or_else(|| "recorded failure".into()),
                };
                return Ok(step_refusal(&err));
            }
        }
        let reconciling = matches!(
            recorded.as_ref().map(|row| row.state),
            Some(ExternalOperationState::Prepared)
        );
        // Write-before-call: the durable Prepared row names the EXACT input
        // identity the restart will reconcile from, BEFORE any remote call
        // (the branch reconciliation included). A write failure refuses the
        // step here — the adapter is never invoked without it.
        if recorded.is_none() {
            let prepared = ExternalOperationRow {
                id: ExternalOperationRow::content_id(
                    &operation_key,
                    provider_name,
                    SCM_PULL_REQUEST_KIND,
                    &input,
                ),
                operation_key: operation_key.clone(),
                provider: provider_name.to_string(),
                kind: SCM_PULL_REQUEST_KIND.to_string(),
                input: input.clone(),
                state: ExternalOperationState::Prepared,
                remote_object_id: None,
                remote_object_version: None,
                started_at: op.handle.now_ms(),
                reconciled_at: None,
            };
            if let Err(e) = op.handle.ledger_external_operation_set(&prepared) {
                let err = ExternalOperationError::Store {
                    operation_key: operation_key.clone(),
                    detail: format!("{e} (no remote call was made)"),
                };
                return Ok(step_refusal(&err));
            }
        }
        #[cfg(test)]
        if self.pr_crash == Some(PrOperationCrashPoint::BeforeRemoteCall) {
            return Err(CompletionStepError::InjectedCrash("before remote call"));
        }
        let operation_id = match faktor_scm::ExternalOperationId::try_new(operation_key.clone()) {
            Ok(operation_id) => operation_id,
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "native PR refused (scm_invalid_input): operation key {operation_key:?} is \
                         not a valid SCM operation identity: {e}"
                    ),
                });
            }
        };
        // The ONE adapter call reconciles the branch at the exact verified
        // head and then create-or-reconciles the PR matching the exact
        // (repository, head, base, marker) identity, all under the recorded
        // operation identity. It maps onto the canonical `faktor_scm`
        // provider types; production runs the real GitHub App adapter.
        let request = CompletionPrRequest {
            operation_id,
            organization,
            repository,
            branch: branch.clone(),
            base: self.config.base_branch.clone(),
            head_sha: head_sha.clone(),
            marker: marker.clone(),
            title: commit_message(&ctx.goal),
            body: format!(
                "Created by Faktor task {}. Reconciliation marker: {marker}",
                op.task_id
            ),
        };
        let remote = match provider.reconcile_completion_pr(&request).await {
            Ok(remote) => remote,
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("native PR refused (scm_provider): {e} [{}]", e.code()),
                });
            }
        };
        // The adapter reconciles the branch before the PR; the reported head
        // must still be the exact verified head (defense in depth: a provider
        // returning a moved branch instead of conflicting is refused).
        if remote.branch.head_sha != head_sha {
            return Ok(StepExecution::Failed {
                detail: format!(
                    "native PR refused (external_operation_remote_object_mismatch): branch \
                     {branch} is at remote head {} but the verified head is {head_sha}",
                    remote.branch.head_sha
                ),
            });
        }
        let remote_object_id = remote.pull_request.reference.number().to_string();
        let remote_object_version = remote.pull_request.version.clone();
        // A recorded remote identity may only be CONFIRMED, never silently
        // replaced: id and version must match exactly.
        if let Some(row) = &recorded {
            if let (Some(recorded_id), Some(recorded_version)) = (
                row.remote_object_id.as_deref(),
                row.remote_object_version.as_deref(),
            ) {
                if recorded_id != remote_object_id.as_str()
                    || recorded_version != remote_object_version.as_str()
                {
                    let err = ExternalOperationError::RemoteObjectMismatch {
                        operation_key: operation_key.clone(),
                        detail: format!(
                            "recorded object {recorded_id}@{recorded_version} but the provider \
                             reports {remote_object_id}@{remote_object_version}"
                        ),
                    };
                    return Ok(step_refusal(&err));
                }
            }
        }
        #[cfg(test)]
        if self.pr_crash == Some(PrOperationCrashPoint::AfterRemoteCallBeforeRecord) {
            return Err(CompletionStepError::InjectedCrash(
                "after remote call before completion record",
            ));
        }
        let completed = ExternalOperationRow {
            id: ExternalOperationRow::content_id(
                &operation_key,
                provider_name,
                SCM_PULL_REQUEST_KIND,
                &input,
            ),
            operation_key: operation_key.clone(),
            provider: provider_name.to_string(),
            kind: SCM_PULL_REQUEST_KIND.to_string(),
            input,
            state: ExternalOperationState::Completed,
            remote_object_id: Some(remote_object_id.clone()),
            remote_object_version: Some(remote_object_version.clone()),
            started_at: recorded
                .as_ref()
                .map(|row| row.started_at)
                .unwrap_or_else(|| op.handle.now_ms()),
            reconciled_at: reconciling.then(|| op.handle.now_ms()),
        };
        if let Err(e) = op.handle.ledger_external_operation_set(&completed) {
            let err = ExternalOperationError::Store {
                operation_key: operation_key.clone(),
                detail: format!("{e} (the remote object identity could not be journaled)"),
            };
            return Ok(step_refusal(&err));
        }
        Ok(StepExecution::Succeeded {
            detail: format!(
                "native PR {remote_object_id}@{remote_object_version} certified via \
                 {provider_name} reconciliation (marker {marker})"
            ),
            pr_url: Some(remote.pull_request.url),
        })
    }
}

/// One typed external-operation refusal as a durable step outcome: the
/// stable machine code always rides the bounded detail.
fn step_refusal(err: &ExternalOperationError) -> StepExecution {
    StepExecution::Failed {
        detail: format!("native PR refused ({}): {err}", err.code()),
    }
}

// ------------------------------------------------------------------ helpers

/// The bounded, deterministic commit message derived from the goal:
/// `faktor: <goal>` truncated on a UTF-8 boundary; an empty goal gets the
/// documented fallback so a commit is never messageless.
pub fn commit_message(goal: &str) -> String {
    let goal = goal.trim();
    let base = if goal.is_empty() {
        "complete the task".to_string()
    } else {
        goal.to_string()
    };
    truncate_bytes(&format!("faktor: {base}"), MAX_COMMIT_MESSAGE_BYTES)
}

/// TRUE when the remote URL names a real network destination that must be
/// consulted through the egress policy. Local paths (absolute, relative,
/// Windows drive) and `file://` URLs are NOT egress; scp-like
/// `user@host:path` and every network scheme are.
pub fn is_egress_destination(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() {
        return false;
    }
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ssh" | "git" | "git+ssh" | "ftp" | "ftps"
        );
    }
    if url.starts_with("file:")
        || url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
    {
        return false;
    }
    // Windows drive path (`C:\...`) or a UNC path stays local.
    if url.len() >= 2 && url.as_bytes()[1] == b':' && url.as_bytes()[0].is_ascii_alphabetic() {
        return false;
    }
    // scp-like shorthand (`user@host:path`) is a network destination.
    url.contains('@') && url.contains(':')
}

/// Render one strict PR template. The template was validated at config time
/// (known placeholders, no control bytes, `{branch}`
/// present); the rendered command is split on whitespace into program+args
/// and is NEVER passed through a shell.
pub fn render_pr_command(
    template: &str,
    branch: &str,
    base: &str,
    remote: &str,
) -> Result<(String, Vec<String>), String> {
    validate_pr_command(template)?;
    let rendered = template
        .replace("{branch}", branch)
        .replace("{base}", base)
        .replace("{remote}", remote);
    if rendered.contains('{') || rendered.contains('}') {
        return Err("rendered pr_command still carries an unresolved placeholder".into());
    }
    let mut parts = rendered.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| "pr_command renders to an empty program".to_string())?
        .to_string();
    Ok((program, parts.map(str::to_string).collect()))
}

/// Strict PR-template validation (DEPRECATED shape): bounded, single-line,
/// no shell metacharacters (no shell is ever involved, and the template is
/// split on whitespace), known placeholders only and at least `{branch}`
/// (the head the PR is created from).
pub fn validate_pr_command(template: &str) -> Result<(), String> {
    if template.trim().is_empty() {
        return Err("pr_command must not be empty".into());
    }
    if template.len() > MAX_PR_COMMAND_BYTES {
        return Err(format!(
            "pr_command of {} bytes exceeds MAX_PR_COMMAND_BYTES ({MAX_PR_COMMAND_BYTES})",
            template.len()
        ));
    }
    for c in template.chars() {
        if c.is_control() {
            return Err("pr_command must not contain control characters or newlines".into());
        }
        if matches!(
            c,
            '|' | '&'
                | ';'
                | '<'
                | '>'
                | '`'
                | '$'
                | '"'
                | '\''
                | '*'
                | '?'
                | '!'
                | '#'
                | '('
                | ')'
        ) {
            return Err(format!(
                "pr_command contains the shell metacharacter {c:?}; the command runs without a shell, so metacharacters are refused (escape it as an argv word instead)"
            ));
        }
    }
    scan_pr_placeholders(template, "pr_command")?;
    if !template.contains("{branch}") {
        return Err("pr_command must template {branch} (the PR head is never implicit)".into());
    }
    Ok(())
}

/// Strict typed-argv validation (P3): `pr_program` plus each `pr_args`
/// element are bounded, single-line (no control characters) argv values with
/// known placeholders only (per element) and `{branch}` somewhere. Spaces
/// are LEGAL — argv elements are executed directly, never split or shelled.
pub fn validate_pr_argv(program: &str, args: &[String]) -> Result<(), String> {
    if program.is_empty() {
        return Err("pr_program must not be empty".into());
    }
    if program.len() > MAX_PR_COMMAND_BYTES {
        return Err(format!(
            "pr_program of {} bytes exceeds MAX_PR_COMMAND_BYTES ({MAX_PR_COMMAND_BYTES})",
            program.len()
        ));
    }
    if args.len() > MAX_PR_ARGS {
        return Err(format!(
            "pr_args of {} elements exceeds MAX_PR_ARGS ({MAX_PR_ARGS})",
            args.len()
        ));
    }
    let mut branch_seen = false;
    for element in std::iter::once(program).chain(args.iter().map(String::as_str)) {
        for c in element.chars() {
            if c.is_control() {
                return Err(
                    "pr_program/pr_args must not contain control characters or newlines".into(),
                );
            }
        }
        branch_seen |= scan_pr_placeholders(element, "pr_program/pr_args")?;
    }
    if !branch_seen {
        return Err(
            "pr_program/pr_args must template {branch} (the PR head is never implicit)".into(),
        );
    }
    Ok(())
}

/// Scan one template element for the documented placeholders; returns TRUE
/// when `{branch}` occurs. Unknown names, a stray `}` and an unclosed `{`
/// are typed refusals.
fn scan_pr_placeholders(template: &str, label: &str) -> Result<bool, String> {
    let bytes = template.as_bytes();
    let mut index = 0;
    let mut branch_seen = false;
    while index < bytes.len() {
        match bytes[index] {
            b'{' => {
                let end = bytes[index + 1..]
                    .iter()
                    .position(|b| *b == b'}')
                    .map(|p| index + 1 + p)
                    .ok_or_else(|| format!("{label} carries an unclosed '{{'"))?;
                let name = &template[index + 1..end];
                if !matches!(name, "branch" | "base" | "remote") {
                    return Err(format!(
                        "{label} placeholder {{{name}}} is unknown; supported: {{branch}}, {{base}}, {{remote}}"
                    ));
                }
                branch_seen |= name == "branch";
                index = end + 1;
            }
            b'}' => return Err(format!("{label} carries a stray '}}'")),
            _ => index += 1,
        }
    }
    Ok(branch_seen)
}

/// Render one typed argv (P3): validate, substitute each element's
/// `{branch}`/`{base}`/`{remote}` independently, and return the program plus
/// the argument vector exactly as the supervisor must spawn it (spaces
/// inside an element are preserved verbatim).
pub fn render_pr_argv(
    program: &str,
    args: &[String],
    branch: &str,
    base: &str,
    remote: &str,
) -> Result<(String, Vec<String>), String> {
    validate_pr_argv(program, args)?;
    let render = |element: &str| {
        element
            .replace("{branch}", branch)
            .replace("{base}", base)
            .replace("{remote}", remote)
    };
    let rendered_program = render(program);
    if rendered_program.contains('{') || rendered_program.contains('}') {
        return Err("rendered pr_program still carries an unresolved placeholder".into());
    }
    if rendered_program.is_empty() {
        return Err("pr_program renders to an empty program".into());
    }
    let rendered_args: Vec<String> = args.iter().map(|arg| render(arg)).collect();
    Ok((rendered_program, rendered_args))
}

fn validate_remote_name(remote: &str) -> Result<(), String> {
    if remote.is_empty() || remote.len() > 128 {
        return Err("remote name must be 1..=128 bytes".into());
    }
    if remote.starts_with('-')
        || remote.contains("..")
        || remote.contains('/')
        || remote.contains('\\')
    {
        return Err(format!("remote name {remote:?} rejected"));
    }
    for c in remote.chars() {
        if c.is_control() || c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[') {
            return Err(format!("remote name {remote:?} rejected"));
        }
    }
    Ok(())
}

fn validate_branch_name(branch: &str) -> Result<(), String> {
    if branch.is_empty() || branch.len() > 128 {
        return Err("base branch must be 1..=128 bytes".into());
    }
    if branch.starts_with('-') || branch.contains("..") {
        return Err(format!("base branch {branch:?} rejected"));
    }
    for c in branch.chars() {
        if c.is_control()
            || c.is_whitespace()
            || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        {
            return Err(format!("base branch {branch:?} rejected"));
        }
    }
    Ok(())
}

/// The FIRST `http(s)://…` URL in the output, trailing punctuation trimmed.
fn parse_pr_url(output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        let Some(start) = token.find("https://").or_else(|| token.find("http://")) else {
            continue;
        };
        let candidate = token[start..].trim_end_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | ')' | ']' | '}' | '>' | '"' | '\''
            )
        });
        if candidate.len() > "https://".len() {
            return Some(candidate.to_string());
        }
    }
    None
}

fn note_suffix(note: &str) -> String {
    if note.is_empty() {
        String::new()
    } else {
        format!(": {note}")
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn bounded_detail(detail: &str) -> String {
    truncate_bytes(detail, MAX_COMPLETION_STEP_DETAIL)
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// The deterministic in-process [`CompletionScm`] test double: an idempotent
/// remote registry keyed by the EXACT (repository, head/base, marker)
/// identity that counts remote creations, so a duplicate-creation bug is
/// observable, and records every request so the orchestrator's mapping
/// (organization/repository/branch/base/head/marker/title/body/operation id)
/// is assertable. It is deliberately NOT a production adapter: production
/// runs `faktor_scm::GitHubCompletionScm` over the real GitHub App adapter —
/// this fake exists so the orchestration protocol itself is provable without
/// a network or credentials; the adapter-to-provider mapping is covered by
/// `crates/scm/tests/completion_adapter.rs`.
#[cfg(test)]
pub(crate) mod scm_fake {
    use faktor_scm::{
        CompletionPrRequest, CompletionPrResult, CompletionScm, PullRequestRef, RemoteRef,
        RepositoryRef, ScmBranch, ScmError, ScmInstallationId, ScmPullRequest,
    };
    use std::sync::{Arc, Mutex};

    const FAKE_INSTALLATION: u64 = 7;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct PrKey {
        organization: String,
        repository: String,
        head: String,
        base: String,
        marker: String,
    }

    #[derive(Debug, Clone)]
    struct FakePr {
        key: PrKey,
        number: u64,
        version: String,
        url: String,
    }

    #[derive(Debug, Clone)]
    struct FakeBranch {
        organization: String,
        repository: String,
        branch: String,
        head_sha: String,
        url: String,
    }

    #[derive(Debug, Default)]
    struct FakeState {
        branch_calls: usize,
        pr_calls: usize,
        remote_creations: usize,
        prs: Vec<FakePr>,
        branches: Vec<FakeBranch>,
        requests: Vec<CompletionPrRequest>,
        pr_version_override: Option<String>,
        branch_head_override: Option<String>,
        transport_failure: Option<String>,
    }

    /// The fake adapter handle (share the same `Arc` across "restarts" so the
    /// remote state survives exactly as a real provider's would).
    #[derive(Debug, Default)]
    pub(crate) struct FakeScmProvider {
        state: Mutex<FakeState>,
    }

    impl FakeScmProvider {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        pub(crate) fn pr_calls(&self) -> usize {
            self.state.lock().expect("fake scm lock").pr_calls
        }

        pub(crate) fn branch_calls(&self) -> usize {
            self.state.lock().expect("fake scm lock").branch_calls
        }

        /// How many pull requests the provider CREATED (a reconciliation
        /// returning the existing object does not increment this).
        pub(crate) fn remote_creations(&self) -> usize {
            self.state.lock().expect("fake scm lock").remote_creations
        }

        pub(crate) fn pull_request_count(&self) -> usize {
            self.state.lock().expect("fake scm lock").prs.len()
        }

        /// Every request the runner handed to the adapter, in order.
        pub(crate) fn requests(&self) -> Vec<CompletionPrRequest> {
            self.state.lock().expect("fake scm lock").requests.clone()
        }

        /// Force every future PR reconciliation to report `version` — the
        /// remote moved under us (force-push/edited PR).
        pub(crate) fn force_pull_request_version(&self, version: &str) {
            self.state
                .lock()
                .expect("fake scm lock")
                .pr_version_override = Some(version.to_string());
        }

        /// Force every future branch reconciliation to report `head` — a
        /// provider that returns a moved branch instead of a conflict (the
        /// orchestrator's defense-in-depth refusal).
        pub(crate) fn force_branch_head(&self, head: &str) {
            self.state
                .lock()
                .expect("fake scm lock")
                .branch_head_override = Some(head.to_string());
        }

        /// Fail every reconciliation with a typed transport error (the
        /// provider-seam error mapping).
        pub(crate) fn fail_reconciliation(&self, detail: &str) {
            self.state.lock().expect("fake scm lock").transport_failure = Some(detail.to_string());
        }

        fn repository(request: &CompletionPrRequest) -> RepositoryRef {
            RepositoryRef::try_new(
                ScmInstallationId::try_from_raw(FAKE_INSTALLATION).expect("fake installation"),
                request.organization.clone(),
                request.repository.clone(),
            )
            .expect("fake repository")
        }
    }

    #[async_trait::async_trait]
    impl CompletionScm for FakeScmProvider {
        fn provider_name(&self) -> &'static str {
            "github"
        }

        async fn reconcile_completion_pr(
            &self,
            request: &CompletionPrRequest,
        ) -> Result<CompletionPrResult, ScmError> {
            let mut state = self.state.lock().expect("fake scm lock");
            state.requests.push(request.clone());
            if let Some(detail) = state.transport_failure.clone() {
                return Err(ScmError::Transport(detail));
            }
            state.branch_calls += 1;
            let position = state.branches.iter().position(|branch| {
                branch.organization == request.organization
                    && branch.repository == request.repository
                    && branch.branch == request.branch
            });
            let (head_sha, branch_url) = match position {
                Some(index) => {
                    let override_head = state.branch_head_override.clone();
                    let branch = &mut state.branches[index];
                    if let Some(head) = override_head {
                        branch.head_sha = head;
                    }
                    (branch.head_sha.clone(), branch.url.clone())
                }
                None => {
                    let head_sha = state
                        .branch_head_override
                        .clone()
                        .unwrap_or_else(|| request.head_sha.clone());
                    let url = format!(
                        "https://github.com/{}/{}/tree/{}",
                        request.organization, request.repository, request.branch
                    );
                    state.branches.push(FakeBranch {
                        organization: request.organization.clone(),
                        repository: request.repository.clone(),
                        branch: request.branch.clone(),
                        head_sha: head_sha.clone(),
                        url: url.clone(),
                    });
                    (head_sha, url)
                }
            };
            let repository = Self::repository(request);
            let branch = ScmBranch {
                reference: RemoteRef::try_new(
                    repository.clone(),
                    format!("refs/heads/{}", request.branch),
                )
                .expect("fake ref"),
                head_sha,
                url: branch_url,
                created: false,
            };
            // A provider that returns a moved branch instead of conflicting:
            // the orchestrator's defense-in-depth refusal (the PR object is a
            // placeholder the orchestrator never consumes).
            if branch.head_sha != request.head_sha {
                let placeholder = ScmPullRequest {
                    reference: PullRequestRef::try_new(repository.clone(), 1)
                        .expect("placeholder pr"),
                    head: request.branch.clone(),
                    base: request.base.clone(),
                    state: "open".into(),
                    marker: request.marker.clone(),
                    version: "branch-mismatch".into(),
                    url: String::new(),
                    created: false,
                };
                return Ok(CompletionPrResult {
                    repository,
                    branch,
                    pull_request: placeholder,
                });
            }
            state.pr_calls += 1;
            let key = PrKey {
                organization: request.organization.clone(),
                repository: request.repository.clone(),
                head: request.branch.clone(),
                base: request.base.clone(),
                marker: request.marker.clone(),
            };
            let (number, version, url) = match state.prs.iter().find(|pr| pr.key == key) {
                Some(existing) => (
                    existing.number,
                    existing.version.clone(),
                    existing.url.clone(),
                ),
                None => {
                    state.remote_creations += 1;
                    let number = state.remote_creations as u64;
                    let version = "v1".to_string();
                    let url = format!(
                        "https://github.com/{}/{}/pull/{number}",
                        request.organization, request.repository
                    );
                    state.prs.push(FakePr {
                        key,
                        number,
                        version: version.clone(),
                        url: url.clone(),
                    });
                    (number, version, url)
                }
            };
            let version = state.pr_version_override.clone().unwrap_or(version);
            let pull_request = ScmPullRequest {
                reference: PullRequestRef::try_new(repository.clone(), number)
                    .expect("fake pr ref"),
                head: request.branch.clone(),
                base: request.base.clone(),
                state: "open".into(),
                marker: request.marker.clone(),
                version,
                url,
                created: false,
            };
            Ok(CompletionPrResult {
                repository,
                branch,
                pull_request,
            })
        }
    }
}

#[cfg(test)]
#[path = "completion_steps_tests.rs"]
mod tests;
