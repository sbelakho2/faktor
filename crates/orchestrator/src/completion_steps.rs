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
//! - **pr**: the native PR step is CERTIFIED only through the typed
//!   [`ScmProvider`] reconciliation protocol: the exact operation identity
//!   (installation/repository + head/base + a stable Faktor task marker) is
//!   journaled as a durable [`ExternalOperationRow`] BEFORE the remote call,
//!   and the remote object id + version are journaled after it. A crash
//!   mid-operation reconciles from the RECORDED identity through
//!   `create_or_reconcile_*` — never a blind re-create, never a duplicate
//!   remote object; a conflicting recorded input identity and a
//!   remote-object version drift are typed refusals. The generic
//!   `pr_command`/`pr_program` expert command remains executable on the
//!   legacy path but carries NO reconciliation protocol, so it can never
//!   certify the native PR step ([`PR_REQUIRES_TYPED_RECONCILIATION`]).
//!   The real GitHub-App adapter is the recorded follow-up: no HTTP client
//!   lives here, only the trait and the deterministic in-process fake tests
//!   drive.
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

/// The durable external-operation kind of the native pull-request
/// certification.
pub const SCM_PULL_REQUEST_KIND: &str = "pull_request";

// -------------------------------------------------- typed SCM reconciliation

/// The provider-neutral repository identity a reconciliation lookup is
/// keyed by. GitHub-App-shaped: the organization/repository pair is exact,
/// the installation id is optional metadata the adapter seam may resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmRepositoryRef {
    pub installation_id: Option<String>,
    pub organization: String,
    pub repository: String,
}

/// One idempotent branch reconciliation request: the provider must return
/// the EXISTING branch object when `branch` already resolves, never fork or
/// duplicate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmBranchSpec {
    pub repository: ScmRepositoryRef,
    pub branch: String,
    /// The exact local head the remote branch must carry.
    pub head_sha: String,
    pub marker: String,
}

/// One idempotent pull-request reconciliation request: the provider MUST
/// look the PR up by (repository, exact head/base, marker) and return the
/// EXISTING object when one matches — a create path that always creates
/// would duplicate the PR after a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmPullRequestSpec {
    pub repository: ScmRepositoryRef,
    pub head: String,
    pub base: String,
    pub marker: String,
    pub title: String,
    pub body: String,
}

/// The identity of one remote object (branch ref or pull request) as the
/// provider reports it. `version` is the provider's opaque version token
/// (for a PR: the head sha + updated-at; for a branch: the head sha): a
/// recorded version that differs from the observed one is a typed refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmRemoteObject {
    pub object_id: String,
    pub version: String,
    pub url: String,
}

/// Typed failure of one SCM provider call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScmError {
    #[error("scm provider refused: {0}")]
    Provider(String),
    #[error("scm remote lookup failed: {0}")]
    Lookup(String),
}

/// The minimal provider-neutral SCM seam of the native PR step. The real
/// GitHub-App adapter (installation-token minting, REST/GraphQL calls,
/// rate-limit handling) is the recorded FOLLOW-UP: no HTTP client is
/// implemented here, and every reconciliation decision is a pure function
/// of the caller's durable [`ExternalOperationRow`] identity, so the fake
/// below and a real adapter are interchangeable.
pub trait ScmProvider: Send + Sync {
    /// The stable provider name folded into the durable operation key.
    fn provider_name(&self) -> &'static str;
    /// Resolve the provider-side repository identity of one parsed
    /// organization/repository pair: a GitHub-App adapter overrides this to
    /// carry its installation id, the neutral default carries none. The
    /// resolved value is what the durable input identity records and what
    /// the reconciliation lookup is keyed by.
    fn repository_ref(&self, organization: &str, repository: &str) -> ScmRepositoryRef {
        ScmRepositoryRef {
            installation_id: None,
            organization: organization.to_string(),
            repository: repository.to_string(),
        }
    }
    /// Create or reconcile `spec.branch` at `spec.head_sha` (idempotent).
    fn create_or_reconcile_branch(&self, spec: &ScmBranchSpec)
        -> Result<ScmRemoteObject, ScmError>;
    /// Create or reconcile the pull request matching the EXACT
    /// (repository, head, base, marker) identity (idempotent).
    fn create_or_reconcile_pull_request(
        &self,
        spec: &ScmPullRequestSpec,
    ) -> Result<ScmRemoteObject, ScmError>;
    /// Look up one remote ref without creating anything.
    fn remote_ref(
        &self,
        repository: &ScmRepositoryRef,
        reference: &str,
    ) -> Result<Option<ScmRemoteObject>, ScmError>;
}

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

/// Parse one git remote URL into a GitHub repository reference (`None` for
/// anything else — the typed path refuses an unrecognizable remote instead
/// of guessing an organization/repository pair). Accepts
/// `https://github.com/org/repo[.git]`, `ssh://git@github.com/org/repo` and
/// the scp-like `git@github.com:org/repo[.git]`.
pub fn parse_scm_repository(
    url: &str,
    installation_id: Option<String>,
) -> Option<ScmRepositoryRef> {
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
    Some(ScmRepositoryRef {
        installation_id,
        organization: organization.to_string(),
        repository: repository.to_string(),
    })
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
    /// The wired typed SCM reconciliation seam (`None` = no native PR can be
    /// certified; the legacy command path stays available to tests only).
    scm_provider: Option<Arc<dyn ScmProvider>>,
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

    /// Wire the typed SCM reconciliation seam of the native PR step. Without
    /// it no native PR step can ever certify (fail-closed); the real GitHub
    /// App adapter is the recorded follow-up.
    pub fn with_scm_provider(mut self, provider: Arc<dyn ScmProvider>) -> Self {
        self.scm_provider = Some(provider);
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
                    previous.verification_record,
                    actx.proof
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
                }
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
                }
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
                    }
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
                }
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                }
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
                }
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
                    }
                }
            },
        };
        let remote = self.config.remote.clone();
        let url = match self.git.remote_url(root, &remote, owner.clone()).await {
            Ok(None) => {
                return StepExecution::Skipped {
                    detail: "no git remote configured (the repository has no remotes)".into(),
                }
            }
            Ok(Some(url)) => url,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                }
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
                }
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
                        }
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git head read failed: {}", e.message),
                        }
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
                        }
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git head read failed: {}", e.message),
                        }
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
                        }
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
                        }
                    }
                    Err(e) => {
                        return StepExecution::Failed {
                            detail: format!("git HEAD tree read failed: {}", e.message),
                        }
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
                }
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
                    }
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
                }
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                }
            }
        };
        let remote = self.config.remote.clone();
        let url = match self.git.remote_url(&ctx.root, &remote, owner.clone()).await {
            Ok(None) => {
                return StepExecution::Skipped {
                    detail: "no git remote configured (the repository has no remotes)".into(),
                }
            }
            Ok(Some(url)) => url,
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                }
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
                }
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git head read failed: {}", e.message),
                }
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
                }
            }
            Err(e) => {
                return StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                }
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
                    }
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
                    }
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
                }
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
            // Fail-closed: without the typed reconciliation seam nothing can
            // certify the native PR step. An unconfigured PR is the historic
            // retryable Skipped; a configured generic command is a TYPED
            // refusal naming its missing protocol.
            if self.config.pr_command.is_none() && self.config.pr_program.is_none() {
                return Ok(StepExecution::Skipped {
                    detail: "[completion] pr_command is not configured; no PR was created".into(),
                });
            }
            return Ok(StepExecution::Failed {
                detail: format!(
                    "native PR refused ({PR_REQUIRES_TYPED_RECONCILIATION}): the configured \
                     pr_command/pr_program is an expert extension with no typed reconciliation \
                     protocol, so it can never certify the native PR step"
                ),
            });
        };
        let owner = ProcessOwner::Daemon;
        let branch = match self.git.current_branch(&ctx.root, owner.clone()).await {
            Ok(branch) if !branch.is_empty() => branch,
            Ok(_) => {
                return Ok(StepExecution::Failed {
                    detail: "cannot certify the native PR step from a detached HEAD".into(),
                })
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git branch read failed: {}", e.message),
                })
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
                })
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git remote read failed: {}", e.message),
                })
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
                            })
                        }
                        Err(e) => {
                            return Ok(StepExecution::Failed {
                                detail: format!(
                                    "native PR refused: git head read failed: {}",
                                    e.message
                                ),
                            })
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
        let Some(parsed) = parse_scm_repository(&remote_url, None) else {
            return Ok(StepExecution::Failed {
                detail: format!(
                    "cannot certify the native PR step: remote {remote_url} is not a recognizable \
                     GitHub repository reference (organization/repository)"
                ),
            });
        };
        let repository = provider.repository_ref(&parsed.organization, &parsed.repository);
        let head_sha = match self.git.head_sha(&ctx.root, owner).await {
            Ok(Some(sha)) => sha,
            Ok(None) => {
                return Ok(StepExecution::Failed {
                    detail: "cannot certify the native PR step: HEAD is unborn".into(),
                })
            }
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!("git head read failed: {}", e.message),
                })
            }
        };
        let marker = native_pr_marker(op.task_id, op.revision);
        let input = ExternalOperationInput {
            organization: repository.organization.clone(),
            repository: repository.repository.clone(),
            head: branch.clone(),
            base: self.config.base_branch.clone(),
            marker: marker.clone(),
        };
        let operation_key =
            native_pr_operation_key(op.task_id, op.revision, provider.provider_name());
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
        // The branch is an idempotent precondition of the PR. Its
        // reconciliation is part of the same typed protocol and its recorded
        // head must equal the exact local head (a moved branch is a typed
        // refusal, never a silently different PR head).
        let branch_spec = ScmBranchSpec {
            repository: repository.clone(),
            branch: branch.clone(),
            head_sha: head_sha.clone(),
            marker: marker.clone(),
        };
        match provider.create_or_reconcile_branch(&branch_spec) {
            Ok(remote) if remote.version != head_sha => {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "native PR refused (external_operation_remote_object_mismatch): branch \
                         {branch} is at remote version {} but the verified head is {head_sha}",
                        remote.version
                    ),
                })
            }
            Ok(_) => {}
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "native PR refused (scm_provider): branch reconciliation failed: {e}"
                    ),
                })
            }
        }
        // Write-before-call: the durable Prepared row names the EXACT input
        // identity the restart will reconcile from. A write failure refuses
        // the step here — the remote call is never made without it.
        if recorded.is_none() {
            let prepared = ExternalOperationRow {
                id: ExternalOperationRow::content_id(
                    &operation_key,
                    provider.provider_name(),
                    SCM_PULL_REQUEST_KIND,
                    &input,
                ),
                operation_key: operation_key.clone(),
                provider: provider.provider_name().to_string(),
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
        let spec = ScmPullRequestSpec {
            repository: repository.clone(),
            head: branch.clone(),
            base: self.config.base_branch.clone(),
            marker: marker.clone(),
            title: commit_message(&ctx.goal),
            body: format!(
                "Created by Faktor task {}. Reconciliation marker: {marker}",
                op.task_id
            ),
        };
        let remote = match provider.create_or_reconcile_pull_request(&spec) {
            Ok(remote) => remote,
            Err(e) => {
                return Ok(StepExecution::Failed {
                    detail: format!(
                        "native PR refused (scm_provider): pull-request reconciliation failed: {e}"
                    ),
                })
            }
        };
        // A recorded remote identity may only be CONFIRMED, never silently
        // replaced: id and version must match exactly.
        if let Some(row) = &recorded {
            if let (Some(recorded_id), Some(recorded_version)) = (
                row.remote_object_id.as_deref(),
                row.remote_object_version.as_deref(),
            ) {
                if recorded_id != remote.object_id || recorded_version != remote.version {
                    let err = ExternalOperationError::RemoteObjectMismatch {
                        operation_key: operation_key.clone(),
                        detail: format!(
                            "recorded object {recorded_id}@{recorded_version} but the provider \
                             reports {}@{}",
                            remote.object_id, remote.version
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
                provider.provider_name(),
                SCM_PULL_REQUEST_KIND,
                &input,
            ),
            operation_key: operation_key.clone(),
            provider: provider.provider_name().to_string(),
            kind: SCM_PULL_REQUEST_KIND.to_string(),
            input,
            state: ExternalOperationState::Completed,
            remote_object_id: Some(remote.object_id),
            remote_object_version: Some(remote.version),
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
                "native PR {}@{} certified via {} reconciliation (marker {marker})",
                completed.remote_object_id.as_deref().unwrap_or_default(),
                completed
                    .remote_object_version
                    .as_deref()
                    .unwrap_or_default(),
                provider.provider_name(),
            ),
            pr_url: Some(remote.url),
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

/// The deterministic in-process [`ScmProvider`] test double: an idempotent
/// remote registry keyed by the EXACT (repository, head/base, marker)
/// identity that counts remote creations, so a duplicate-creation bug is
/// observable. It is deliberately NOT a production adapter: the real
/// GitHub-App adapter (installation tokens, REST/GraphQL, rate limits) is
/// the recorded follow-up — this fake exists so the reconciliation protocol
/// itself is provable without a network or credentials.
#[cfg(test)]
pub(crate) mod scm_fake {
    use super::{
        ScmBranchSpec, ScmError, ScmProvider, ScmPullRequestSpec, ScmRemoteObject, ScmRepositoryRef,
    };
    use std::sync::{Arc, Mutex};

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
        object: ScmRemoteObject,
    }

    #[derive(Debug, Clone)]
    struct FakeBranch {
        organization: String,
        repository: String,
        branch: String,
        object: ScmRemoteObject,
    }

    #[derive(Debug, Default)]
    struct FakeState {
        branch_calls: usize,
        pr_calls: usize,
        remote_creations: usize,
        prs: Vec<FakePr>,
        branches: Vec<FakeBranch>,
        pr_version_override: Option<String>,
        branch_version_override: Option<String>,
    }

    /// The fake provider handle (share the same `Arc` across "restarts" so
    /// the remote state survives exactly as a real provider's would).
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

        /// Force every future PR reconciliation to report `version` — the
        /// remote moved under us (force-push/edited PR).
        pub(crate) fn force_pull_request_version(&self, version: &str) {
            self.state
                .lock()
                .expect("fake scm lock")
                .pr_version_override = Some(version.to_string());
        }

        /// Force every future branch reconciliation to report `version`.
        pub(crate) fn force_branch_version(&self, version: &str) {
            self.state
                .lock()
                .expect("fake scm lock")
                .branch_version_override = Some(version.to_string());
        }
    }

    impl ScmProvider for FakeScmProvider {
        fn provider_name(&self) -> &'static str {
            "github"
        }

        fn create_or_reconcile_branch(
            &self,
            spec: &ScmBranchSpec,
        ) -> Result<ScmRemoteObject, ScmError> {
            let mut state = self.state.lock().expect("fake scm lock");
            state.branch_calls += 1;
            let position = state.branches.iter().position(|b| {
                b.organization == spec.repository.organization
                    && b.repository == spec.repository.repository
                    && b.branch == spec.branch
            });
            let override_version = state.branch_version_override.clone();
            let mut object = match position {
                Some(index) => {
                    let branch = &mut state.branches[index];
                    if let Some(version) = &override_version {
                        branch.object.version = version.clone();
                    }
                    branch.object.clone()
                }
                None => {
                    let object = ScmRemoteObject {
                        object_id: format!("refs/heads/{}", spec.branch),
                        version: spec.head_sha.clone(),
                        url: format!(
                            "https://github.com/{}/{}/tree/{}",
                            spec.repository.organization, spec.repository.repository, spec.branch
                        ),
                    };
                    state.branches.push(FakeBranch {
                        organization: spec.repository.organization.clone(),
                        repository: spec.repository.repository.clone(),
                        branch: spec.branch.clone(),
                        object: object.clone(),
                    });
                    object
                }
            };
            if let Some(version) = override_version {
                object.version = version;
            }
            Ok(object)
        }

        fn create_or_reconcile_pull_request(
            &self,
            spec: &ScmPullRequestSpec,
        ) -> Result<ScmRemoteObject, ScmError> {
            let mut state = self.state.lock().expect("fake scm lock");
            state.pr_calls += 1;
            let key = PrKey {
                organization: spec.repository.organization.clone(),
                repository: spec.repository.repository.clone(),
                head: spec.head.clone(),
                base: spec.base.clone(),
                marker: spec.marker.clone(),
            };
            let mut object = match state.prs.iter().find(|pr| pr.key == key) {
                Some(existing) => existing.object.clone(),
                None => {
                    state.remote_creations += 1;
                    let number = state.remote_creations;
                    let object = ScmRemoteObject {
                        object_id: format!("pr-{number}"),
                        version: "v1".to_string(),
                        url: format!(
                            "https://github.com/{}/{}/pull/{number}",
                            spec.repository.organization, spec.repository.repository
                        ),
                    };
                    state.prs.push(FakePr {
                        key,
                        object: object.clone(),
                    });
                    object
                }
            };
            if let Some(version) = state.pr_version_override.clone() {
                object.version = version;
            }
            Ok(object)
        }

        fn remote_ref(
            &self,
            repository: &ScmRepositoryRef,
            reference: &str,
        ) -> Result<Option<ScmRemoteObject>, ScmError> {
            let state = self.state.lock().expect("fake scm lock");
            Ok(state
                .branches
                .iter()
                .find(|b| {
                    b.organization == repository.organization
                        && b.repository == repository.repository
                        && b.branch == reference
                })
                .map(|b| b.object.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    //! Adversarial covers of the completion-step runner: no-contract parity,
    //! truthful commit outcomes with a bounded message, Skipped/retryable
    //! gate interplay, egress-denied and remote-less pushes, unconfigured /
    //! configured PR commands (URL parse + already-exists idempotency),
    //! ordering + terminal stop, and reopen idempotency across a simulated
    //! crash between steps.
    use super::*;
    use faktor_core::completion::CompletionContract;
    use faktor_core::id::VerificationRecordId;
    use faktor_core::state::{TaskState, TaskTransition, VerificationStatus};
    use faktor_session::{CompletionContractGate, SessionManager, Task, TaskBudget, TaskError};
    use std::path::Path;

    fn allow_all() -> Arc<dyn EgressPolicy> {
        Arc::new(|_url: &str| Ok(()))
    }

    fn deny_all() -> Arc<dyn EgressPolicy> {
        Arc::new(|url: &str| Err(format!("network denied by test policy for {url}")))
    }

    fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, m)
    }

    fn session_with_task(
        m: &Arc<SessionManager>,
        goal: &str,
        contract: Option<CompletionContract>,
    ) -> (SessionHandle, TaskId) {
        let ws = m.create_workspace("/w").unwrap();
        let h = m
            .create_session(ws, "completion-steps", "fake", "fake")
            .unwrap();
        let task_id = h.task_id().unwrap();
        let now = h.now_ms();
        h.create_task(Task {
            task_id,
            session_id: h.id(),
            goal: goal.into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
        if let Some(contract) = contract {
            let rev = h.task_revision(task_id).unwrap();
            h.set_completion_contract(task_id, rev, contract).unwrap();
        }
        (h, task_id)
    }

    fn runner(
        dir: &Path,
        config: CompletionStepsConfig,
        egress: Arc<dyn EgressPolicy>,
    ) -> CompletionStepRunner {
        let cas = Arc::new(faktor_cas::Cas::open(dir.join("exec-cas")).unwrap());
        let supervisor = ProcessSupervisor::new(cas);
        CompletionStepRunner::new(supervisor, egress, config).unwrap()
    }

    fn git(cwd: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_output(cwd: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn init_repo(root: &Path) -> std::path::PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@faktor.local"]);
        git(&repo, &["config", "user.name", "Faktor Test"]);
        std::fs::write(repo.join("README.md"), "base\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        repo
    }

    fn add_bare_remote(root: &Path, repo: &Path) -> std::path::PathBuf {
        let bare = root.join("remote.git");
        git(root, &["init", "--bare", "-q", bare.to_str().unwrap()]);
        git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
        bare
    }

    fn contract(commit: bool, push: bool, pr: bool) -> CompletionContract {
        CompletionContract {
            include_commit: commit,
            include_push: push,
            include_pr: pr,
        }
    }

    fn ctx(root: &Path, goal: &str) -> CompletionStepContext {
        CompletionStepContext {
            root: root.to_path_buf(),
            goal: goal.to_string(),
        }
    }

    fn step_rows(
        h: &SessionHandle,
        task_id: TaskId,
    ) -> Vec<faktor_session::CompletionStepStatusRow> {
        // Statuses are keyed by the CONTRACT revision, not the task row's
        // current revision (machine transitions bump the latter).
        let (revision, _) = h.completion_contract(task_id).unwrap().expect("contract");
        h.ledger_completion_step_statuses(task_id.raw(), revision.raw())
            .unwrap()
    }

    fn drive_to_verifying(h: &SessionHandle, task_id: TaskId) {
        for transition in [
            TaskTransition::StartRunning,
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ] {
            let rev = h.task_revision(task_id).unwrap();
            h.transition_task(task_id, rev, transition, None).unwrap();
        }
    }

    fn passing_record(h: &SessionHandle, task_id: TaskId) -> VerificationRecordId {
        h.create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            h.now_ms(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn no_contract_is_a_pure_no_op_parity_path() {
        let (dir, m) = manager();
        let (h, task_id) = session_with_task(&m, "no contract", None);
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        // The root does not even exist: if the runner touched git the test
        // would fail — parity means the runner is never invoked.
        let report = runner
            .run(&h, task_id, &ctx(&dir.path().join("nope"), "no contract"))
            .await
            .unwrap();
        assert!(report.records.is_empty());
        assert!(report.pr_url.is_none());
        assert!(h
            .ledger_completion_contract(task_id.raw())
            .unwrap()
            .is_none());
        let rev = h.task_revision(task_id).unwrap();
        assert!(h
            .ledger_completion_step_statuses(task_id.raw(), rev.raw())
            .unwrap()
            .is_empty());
        assert_eq!(
            h.completion_contract_gate(task_id).unwrap(),
            CompletionContractGate::Satisfied
        );
    }

    #[tokio::test]
    async fn commit_step_commits_real_changes_with_a_bounded_goal_message() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        std::fs::write(repo.join("feature.txt"), "new content\n").unwrap();
        let long_goal = format!("implement the feature {}", "x".repeat(500));
        let (h, task_id) = session_with_task(&m, &long_goal, Some(contract(true, false, false)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, &long_goal))
            .await
            .unwrap();
        assert!(report.all_succeeded(), "{report:?}");
        assert!(dir_clean(&repo));
        let message = git_output(&repo, &["log", "-1", "--pretty=%B"]);
        assert!(
            message.starts_with("faktor: implement the feature"),
            "{message}"
        );
        assert!(
            message.len() <= MAX_COMMIT_MESSAGE_BYTES,
            "{}",
            message.len()
        );
        let rows = step_rows(&h, task_id);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].step, CompletionStep::Commit);
        assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
        // Idempotent replay: the status row is not duplicated.
        let replay = runner
            .run(&h, task_id, &ctx(&repo, &long_goal))
            .await
            .unwrap();
        assert_eq!(
            replay.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Succeeded)
        );
        assert_eq!(
            step_rows(&h, task_id).len(),
            1,
            "no duplicate row on replay"
        );
        assert!(dir_clean(&repo));
    }

    fn dir_clean(repo: &Path) -> bool {
        git_output(repo, &["status", "--porcelain"]).is_empty()
    }

    #[tokio::test]
    async fn nothing_to_commit_is_skipped_and_the_gate_treats_it_as_unmet() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let (h, task_id) = session_with_task(&m, "clean tree", Some(contract(true, false, false)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "clean tree"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Skipped),
            "{report:?}"
        );
        let rows = step_rows(&h, task_id);
        assert_eq!(rows[0].status, CompletionStepOutcome::Skipped);
        // The documented gate rule: Skipped is NOT succeeded, so a passing
        // proof still refuses — retryably — and the task stays Verifying.
        drive_to_verifying(&h, task_id);
        let record = passing_record(&h, task_id);
        let rev = h.task_revision(task_id).unwrap();
        let err = h.complete_verified_task(task_id, rev, record).unwrap_err();
        match err {
            TaskError::CompletionStepNotSucceeded { status, step, .. } => {
                assert_eq!(step, CompletionStep::Commit);
                assert_eq!(status, CompletionStepOutcome::Skipped);
            }
            other => panic!("expected a retryable Skipped refusal, got {other}"),
        }
        assert_eq!(
            h.get_task(task_id).unwrap().unwrap().state,
            TaskState::Verifying
        );
        // A later real change + re-run flips the SAME contract revision to
        // Succeeded (Skipped stays retryable) and the gate then certifies.
        std::fs::write(repo.join("later.txt"), "later\n").unwrap();
        let report = runner
            .run(&h, task_id, &ctx(&repo, "clean tree"))
            .await
            .unwrap();
        assert!(report.all_succeeded(), "{report:?}");
        let rev = h.task_revision(task_id).unwrap();
        let record = passing_record(&h, task_id);
        let done = h.complete_verified_task(task_id, rev, record).unwrap();
        assert_eq!(done.state, TaskState::VerifiedComplete);
    }

    #[tokio::test]
    async fn push_to_a_local_bare_remote_succeeds_and_bypasses_egress() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let bare = add_bare_remote(dir.path(), &repo);
        let (h, task_id) = session_with_task(&m, "push it", Some(contract(false, true, false)));
        // A DENY-ALL policy: local path remotes are not egress, so the push
        // still succeeds (the policy is never consulted).
        let runner = runner(dir.path(), CompletionStepsConfig::default(), deny_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "push it"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Push),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(
            git_output(&bare, &["rev-parse", "main"]),
            head,
            "the bare remote must hold the pushed commit"
        );
        let rows = step_rows(&h, task_id);
        assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
        // Replay: latest row is Succeeded, so no second row and no push.
        let replay = runner
            .run(&h, task_id, &ctx(&repo, "push it"))
            .await
            .unwrap();
        assert!(replay.all_succeeded());
        assert_eq!(step_rows(&h, task_id).len(), 1);
    }

    #[tokio::test]
    async fn push_denied_by_egress_policy_is_a_typed_failure() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://git.example.invalid/team/repo.git",
            ],
        );
        let (h, task_id) = session_with_task(&m, "push denied", Some(contract(false, true, false)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), deny_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "push denied"))
            .await
            .unwrap();
        let failed = report.failed_step();
        assert_eq!(failed, Some(CompletionStep::Push), "{report:?}");
        let rows = step_rows(&h, task_id);
        assert_eq!(rows[0].status, CompletionStepOutcome::Failed);
        assert!(
            rows[0].detail.contains("egress policy denied"),
            "{}",
            rows[0].detail
        );
        // The gate refuses TERMINALLY: even a passing proof cannot certify.
        drive_to_verifying(&h, task_id);
        let record = passing_record(&h, task_id);
        let rev = h.task_revision(task_id).unwrap();
        let err = h.complete_verified_task(task_id, rev, record).unwrap_err();
        assert!(
            matches!(err, TaskError::CompletionStepFailed { .. }),
            "{err}"
        );
        // A re-run never retries a terminally failed step and writes nothing.
        let replay = runner
            .run(&h, task_id, &ctx(&repo, "push denied"))
            .await
            .unwrap();
        assert_eq!(replay.failed_step(), Some(CompletionStep::Push));
        assert_eq!(step_rows(&h, task_id).len(), 1, "terminal rows are frozen");
    }

    #[tokio::test]
    async fn push_without_a_remote_is_skipped() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let (h, task_id) = session_with_task(&m, "no remote", Some(contract(false, true, false)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "no remote"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Push),
            Some(CompletionStepOutcome::Skipped),
            "{report:?}"
        );
        let rows = step_rows(&h, task_id);
        assert!(rows[0].detail.contains("no git remote configured"));
    }

    #[tokio::test]
    async fn detached_head_push_is_failed_typed() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        add_bare_remote(dir.path(), &repo);
        git(&repo, &["checkout", "-q", "--detach"]);
        let (h, task_id) = session_with_task(&m, "detached", Some(contract(false, true, false)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "detached"))
            .await
            .unwrap();
        assert_eq!(
            report.failed_step(),
            Some(CompletionStep::Push),
            "{report:?}"
        );
        assert!(step_rows(&h, task_id)[0].detail.contains("detached HEAD"));
    }

    #[tokio::test]
    async fn unconfigured_pr_command_is_skipped() {
        let (dir, m) = manager();
        let (h, task_id) =
            session_with_task(&m, "no pr command", Some(contract(false, false, true)));
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&dir.path().join("nope"), "no pr command"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Skipped),
            "{report:?}"
        );
        assert!(step_rows(&h, task_id)[0]
            .detail
            .contains("pr_command is not configured"));
    }

    #[cfg(unix)]
    fn fake_pr_script(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-pr.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn configured_pr_command_succeeds_and_parses_the_url() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let script = fake_pr_script(dir.path(), "echo \"https://example.test/pr/42\"");
        let config = CompletionStepsConfig {
            pr_command: Some(format!("{} {{branch}} {{base}}", script.display())),
            ..Default::default()
        };
        let (h, task_id) = session_with_task(&m, "open a pr", Some(contract(false, false, true)));
        let runner = runner(dir.path(), config, allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "open a pr"))
            .await
            .unwrap();
        assert!(report.all_succeeded(), "{report:?}");
        assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/42"));
        assert!(step_rows(&h, task_id)[0]
            .detail
            .contains("https://example.test/pr/42"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn typed_argv_pr_program_executes_with_spaces_preserved() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        // A program path WITH SPACES and an argument WITH SPACES: the typed
        // argv is executed directly (no shell, no whitespace splitting), so
        // each element must arrive verbatim. The spy writes its argv to
        // `args.txt` in the step's cwd (the repo root).
        use std::os::unix::fs::PermissionsExt;
        let tools = dir.path().join("my tools");
        std::fs::create_dir_all(&tools).unwrap();
        let script = tools.join("fake pr.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > argv-spy.txt\necho \"https://example.test/pr/11\"\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        let config = CompletionStepsConfig {
            pr_program: Some(script.display().to_string()),
            pr_args: vec![
                "pr".into(),
                "create".into(),
                "--head".into(),
                "{branch}".into(),
                "--title".into(),
                "spaces stay one arg".into(),
                "--remote".into(),
                "{remote}".into(),
            ],
            base_branch: "main".into(),
            ..Default::default()
        };
        let (h, task_id) =
            session_with_task(&m, "typed argv pr", Some(contract(false, false, true)));
        let runner = runner(dir.path(), config, allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "typed argv pr"))
            .await
            .unwrap();
        assert!(report.all_succeeded(), "{report:?}");
        assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/11"));
        let observed = std::fs::read_to_string(repo.join("argv-spy.txt")).unwrap();
        assert_eq!(
            observed, "pr\ncreate\n--head\nmain\n--title\nspaces stay one arg\n--remote\norigin\n",
            "every argv element must arrive verbatim, spaces included"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_pr_is_an_idempotent_success_with_the_url() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let script = fake_pr_script(
            dir.path(),
            "echo \"PR already exists: https://example.test/pr/7\"\nexit 1",
        );
        let config = CompletionStepsConfig {
            pr_command: Some(format!("{} {{branch}}", script.display())),
            ..Default::default()
        };
        let (h, task_id) = session_with_task(&m, "existing pr", Some(contract(false, false, true)));
        let runner = runner(dir.path(), config, allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "existing pr"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        assert_eq!(report.pr_url.as_deref(), Some("https://example.test/pr/7"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_commit_stops_push_and_pr_and_records_them_skipped() {
        let (dir, m) = manager();
        // A repo whose pre-commit hook REJECTS: the commit fails
        // deterministically on every machine (never reliant on a missing
        // global git identity).
        let repo = init_repo(dir.path());
        std::fs::write(repo.join("b.txt"), "will-not-commit").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = repo.join(".git/hooks/pre-commit");
            std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            let mut perms = std::fs::metadata(&hook).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms).unwrap();
        }
        let bare = add_bare_remote(dir.path(), &repo);
        let marker = dir.path().join("pr-ran.marker");
        let script = fake_pr_script(
            dir.path(),
            &format!(
                "echo ran > {}\necho https://example.test/pr/1",
                marker.display()
            ),
        );
        let config = CompletionStepsConfig {
            pr_command: Some(format!("{} {{branch}}", script.display())),
            ..Default::default()
        };
        let (h, task_id) = session_with_task(&m, "ordering", Some(contract(true, true, true)));
        let runner = runner(dir.path(), config, allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "ordering"))
            .await
            .unwrap();
        // Gate order: commit failed; push and pr are recorded Skipped
        // ("not attempted") and never executed.
        assert_eq!(
            report.failed_step(),
            Some(CompletionStep::Commit),
            "{report:?}"
        );
        assert_eq!(
            report
                .records
                .iter()
                .map(|r| (r.step, r.status))
                .collect::<Vec<_>>(),
            vec![
                (CompletionStep::Commit, CompletionStepOutcome::Failed),
                (CompletionStep::Push, CompletionStepOutcome::Skipped),
                (CompletionStep::Pr, CompletionStepOutcome::Skipped),
            ]
        );
        assert!(
            !marker.exists(),
            "the PR command must not run after a failed commit"
        );
        assert!(
            std::process::Command::new("git")
                .args(["rev-parse", "--verify", "main"])
                .current_dir(&bare)
                .output()
                .unwrap()
                .status
                .code()
                != Some(0),
            "the push must not run after a failed commit"
        );
        assert!(step_rows(&h, task_id).iter().all(|r| !r.detail.is_empty()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn crash_between_steps_reopens_and_completes_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let cas = dir.path().join("cas");
        let repo = init_repo(dir.path());
        std::fs::write(repo.join("work.txt"), "committed before the crash\n").unwrap();
        let contract = contract(true, true, false);
        let task_id;
        {
            // Phase 1: commit succeeds, push is skipped (no remote yet) —
            // the "crash" happens between the two steps.
            let m = SessionManager::open(&store, &cas, true).unwrap();
            let (h, tid) = session_with_task(&m, "crash between steps", Some(contract));
            task_id = tid;
            let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
            let report = runner
                .run(&h, tid, &ctx(&repo, "crash between steps"))
                .await
                .unwrap();
            assert_eq!(
                report.outcome_of(CompletionStep::Commit),
                Some(CompletionStepOutcome::Succeeded)
            );
            assert_eq!(
                report.outcome_of(CompletionStep::Push),
                Some(CompletionStepOutcome::Skipped)
            );
        }
        // The remote comes up while the "daemon is down".
        add_bare_remote(dir.path(), &repo);
        // Phase 2: reopen the SAME store and re-run: the commit is NOT
        // re-executed (its durable row is already Succeeded) and the push
        // completes; the gate is then satisfied.
        let m = SessionManager::open(&store, &cas, true).unwrap();
        let h = m
            .get_session(faktor_core::id::SessionId::new(1))
            .unwrap()
            .unwrap();
        {
            let rev = h.task_revision(task_id).unwrap();
            let rows = h
                .ledger_completion_step_statuses(task_id.raw(), rev.raw())
                .unwrap();
            assert_eq!(rows.len(), 2, "durable rows survived the reopen: {rows:?}");
        }
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run(&h, task_id, &ctx(&repo, "crash between steps"))
            .await
            .unwrap();
        assert!(report.all_succeeded(), "{report:?}");
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Succeeded)
        );
        let rev = h.task_revision(task_id).unwrap();
        let rows = h
            .ledger_completion_step_statuses(task_id.raw(), rev.raw())
            .unwrap();
        assert_eq!(
            rows.iter()
                .filter(|r| r.step == CompletionStep::Commit)
                .count(),
            1,
            "the committed step is never re-recorded on replay: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.step == CompletionStep::Push)
                .count(),
            2,
            "the retried Skipped push appends the Succeeded row: {rows:?}"
        );
        assert_eq!(
            h.completion_contract_gate(task_id).unwrap(),
            CompletionContractGate::Satisfied
        );
        assert!(dir_clean(&repo));
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        let bare = dir.path().join("remote.git");
        assert_eq!(git_output(&bare, &["rev-parse", "main"]), head);
    }

    #[test]
    fn strict_config_validation_and_template_rendering() {
        assert!(CompletionStepsConfig::default().validate().is_ok());
        let bad = |command: &str| {
            CompletionStepsConfig {
                pr_command: Some(command.into()),
                ..Default::default()
            }
            .validate()
            .is_err()
        };
        assert!(bad(""));
        assert!(bad("gh pr create --head {branch}; rm -rf /"));
        assert!(bad("gh pr create --head {branch} && true"));
        assert!(bad("gh pr create --head {branch} $(whoami)"));
        assert!(bad("gh pr create --head {nope}"));
        assert!(bad("gh pr create --head main"));
        assert!(bad("gh pr create\n--head {branch}"));
        assert!(bad(&format!(
            "gh pr create --head {{branch}} {}",
            "x".repeat(MAX_PR_COMMAND_BYTES)
        )));
        assert!(CompletionStepsConfig {
            remote: "a b".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(CompletionStepsConfig {
            base_branch: "a..b".into(),
            ..Default::default()
        }
        .validate()
        .is_err());
        let (program, args) = render_pr_command(
            "gh pr create --head {branch} --base {base}",
            "feat/x",
            "main",
            "origin",
        )
        .unwrap();
        assert_eq!(program, "gh");
        assert_eq!(
            args,
            vec!["pr", "create", "--head", "feat/x", "--base", "main"]
        );
        assert!(!is_egress_destination("/tmp/remote.git"));
        assert!(!is_egress_destination("file:///tmp/remote.git"));
        assert!(!is_egress_destination(r"C:\repos\remote.git"));
        assert!(is_egress_destination("https://github.com/o/r.git"));
        assert!(is_egress_destination("git@github.com:o/r.git"));
        assert_eq!(
            parse_pr_url("see https://example.test/pr/1)."),
            Some("https://example.test/pr/1".to_string())
        );
        assert!(parse_pr_url("no url here").is_none());
    }

    /// P3 typed argv: per-element placeholder substitution (spaces are legal
    /// argv characters), strict refusals, and legacy-template parity with
    /// the exact historic security checks.
    #[test]
    fn typed_argv_pr_config_substitutes_per_element_and_keeps_legacy_parity() {
        let program = "/opt/My Tools/gh";
        let args: Vec<String> = [
            "pr",
            "create",
            "--head",
            "{branch}",
            "--base",
            "{base}",
            "--title",
            "my PR title",
            "{remote}",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let config = CompletionStepsConfig {
            pr_program: Some(program.into()),
            pr_args: args.clone(),
            ..Default::default()
        };
        config.validate().expect("spaces are legal argv characters");
        let (rendered_program, rendered_args) =
            render_pr_argv(program, &args, "feat/x", "main", "origin").unwrap();
        assert_eq!(rendered_program, program, "the program path stays exact");
        assert_eq!(
            rendered_args,
            vec![
                "pr",
                "create",
                "--head",
                "feat/x",
                "--base",
                "main",
                "--title",
                "my PR title",
                "origin"
            ],
            "every element substitutes independently; spaces are preserved"
        );

        // Strict refusals: empty program, unknown/unclosed/stray placeholder,
        // missing {branch}, control chars, over-bound args, args without a
        // program, and both shapes configured at once.
        let bad = |program: Option<&str>, args: Vec<&str>| {
            CompletionStepsConfig {
                pr_program: program.map(str::to_string),
                pr_args: args.into_iter().map(str::to_string).collect(),
                ..Default::default()
            }
            .validate()
            .is_err()
        };
        assert!(bad(Some(""), vec!["{branch}"]));
        assert!(bad(Some("/bin/gh"), vec!["--head", "{nope}"]));
        assert!(bad(Some("/bin/gh"), vec!["--head", "{branch"]));
        assert!(bad(Some("/bin/gh"), vec!["--head", "{branch}}"]));
        assert!(bad(Some("/bin/gh"), vec!["--head", "main"]));
        assert!(bad(Some("/bin/gh"), vec!["--head", "{branch}\u{7}"]));
        let too_many: Vec<&str> = std::iter::repeat_n("{branch}", MAX_PR_ARGS + 1).collect();
        assert!(bad(Some("/bin/gh"), too_many));
        assert!(bad(None, vec!["{branch}"]));
        assert!(CompletionStepsConfig {
            pr_command: Some("gh pr create --head {branch}".into()),
            pr_program: Some("/bin/gh".into()),
            ..Default::default()
        }
        .validate()
        .is_err());

        // Legacy parity: the historic render is byte-identical and every
        // historic refusal still fires.
        let (legacy_program, legacy_args) = render_pr_command(
            "gh pr create --head {branch} --base {base}",
            "feat/x",
            "main",
            "origin",
        )
        .unwrap();
        assert_eq!(legacy_program, "gh");
        assert_eq!(
            legacy_args,
            vec!["pr", "create", "--head", "feat/x", "--base", "main"]
        );
        for invalid in [
            "",
            "gh pr create --head {branch}; rm -rf /",
            "gh pr create --head {branch} && true",
            "gh pr create --head {branch} $(whoami)",
            "gh pr create --head {nope}",
            "gh pr create --head main",
            "gh pr create\n--head {branch}",
        ] {
            assert!(
                validate_pr_command(invalid).is_err(),
                "{invalid:?} must stay refused"
            );
        }
        assert!(
            validate_pr_argv("gh pr create --head {branch}; rm -rf /", &[]).is_ok(),
            "typed argv never splits or shells: spaces/metacharacters are plain characters"
        );
    }

    // ------------------- verified-root race injection (P0 completion gates)

    fn repo_tree(repo: &Path) -> String {
        faktor_fs::tree_manifest::tree_manifest_digest(
            repo,
            faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
        )
        .unwrap()
    }

    /// A session whose workspace root IS `root`, with a durable integration
    /// record whose final snapshot equals `root`'s current digest — exactly
    /// the world an orchestrated verified run lives in.
    fn session_bound_to_root(
        m: &Arc<SessionManager>,
        root: &Path,
        goal: &str,
        contract: Option<CompletionContract>,
    ) -> (SessionHandle, TaskId, String) {
        let ws = m.create_workspace(root.to_str().unwrap()).unwrap();
        let h = m
            .create_session(ws, "completion-race", "fake", "fake")
            .unwrap();
        let task_id = h.task_id().unwrap();
        let now = h.now_ms();
        h.create_task(Task {
            task_id,
            session_id: h.id(),
            goal: goal.into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
        if let Some(contract) = contract {
            let rev = h.task_revision(task_id).unwrap();
            h.set_completion_contract(task_id, rev, contract).unwrap();
        }
        let tree = repo_tree(root);
        h.ledger_integration_record_set(&faktor_session::IntegrationRecordRow {
            run_id: "run-race".into(),
            task_id: task_id.raw(),
            base_revision: None,
            base_snapshot: None,
            run_base_snapshot: None,
            candidate_snapshot: None,
            landed_snapshot: Some(tree.clone()),
            proof_basis_digest: None,
            integration_txn_id: None,
            final_root: root.to_string_lossy().into_owned(),
            final_snapshot_hash: tree.clone(),
            integrated_files: vec![],
            integrated_file_count: 0,
            integrated_files_digest: String::new(),
            conflicts: vec![],
            conflict_count: 0,
            sources: vec![],
            source_count: 0,
            sources_digest: String::new(),
            at_ms: now,
        })
        .unwrap();
        (h, task_id, tree)
    }

    fn passing_record_with_tree(
        h: &SessionHandle,
        task_id: TaskId,
        tree: &str,
    ) -> VerificationRecordId {
        h.create_verification_record(
            task_id,
            Some(tree.to_string()),
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            h.now_ms(),
        )
        .unwrap()
    }

    fn assert_task_stays_verifying_and_steps_invalidated(h: &SessionHandle, task_id: TaskId) {
        assert_eq!(
            h.get_task(task_id).unwrap().unwrap().state,
            TaskState::Verifying,
            "an invalidated completion must never complete the task"
        );
        // The gate reports the unmet step EITHER as Invalidated (the step
        // itself was refused before its side effect) or as the durable
        // snapshot mismatch of an already-succeeded step whose root moved —
        // both are RETRYABLE refusals, never a terminal failure.
        match h.completion_contract_gate(task_id).unwrap() {
            CompletionContractGate::Refused(TaskError::CompletionStepNotSucceeded {
                status,
                ..
            }) => assert_eq!(status, CompletionStepOutcome::Invalidated),
            CompletionContractGate::Refused(TaskError::CompletionStepSnapshotMismatch {
                ..
            }) => {}
            other => panic!("expected a retryable refusal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn injected_edit_after_verification_before_commit_blocks_the_commit() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, "race commit", Some(contract(true, false, false)));
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        // The injected edit lands AFTER the proof was minted and BEFORE the
        // commit step runs: the per-step revalidation must refuse.
        let repo_hook = repo.clone();
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
            .with_pre_step_hook(Arc::new(move |step| {
                if step == CompletionStep::Commit {
                    std::fs::write(repo_hook.join("feature.txt"), "injected edit\n").unwrap();
                }
            }));
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race commit"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Invalidated),
            "{report:?}"
        );
        // No commit happened: the init commit is the only one.
        let log = git_output(&repo, &["log", "--oneline"]);
        assert_eq!(log.lines().count(), 1, "no injected commit may land: {log}");
        assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
    }

    #[tokio::test]
    async fn injected_edit_after_commit_before_push_blocks_the_push() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let bare = add_bare_remote(dir.path(), &repo);
        std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, "race push", Some(contract(true, true, false)));
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let repo_hook = repo.clone();
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
            .with_pre_step_hook(Arc::new(move |step| {
                if step == CompletionStep::Push {
                    std::fs::write(repo_hook.join("feature.txt"), "injected after commit\n")
                        .unwrap();
                }
            }));
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race push"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        assert_eq!(
            report.outcome_of(CompletionStep::Push),
            Some(CompletionStepOutcome::Invalidated),
            "{report:?}"
        );
        // The commit landed; the push must NOT have reached the remote.
        assert_eq!(git_output(&repo, &["log", "--oneline"]).lines().count(), 2);
        let pushed = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "main"])
            .current_dir(&bare)
            .output()
            .unwrap();
        assert!(
            pushed.status.code() != Some(0),
            "the remote must have no branch after an invalidated push"
        );
        assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injected_edit_after_push_before_pr_blocks_the_pr() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        add_bare_remote(dir.path(), &repo);
        std::fs::write(repo.join("feature.txt"), "verified content\n").unwrap();
        let marker = dir.path().join("pr-ran.marker");
        let script = fake_pr_script(
            dir.path(),
            &format!(
                "echo ran > {}\necho https://example.test/pr/9",
                marker.display()
            ),
        );
        let config = CompletionStepsConfig {
            pr_command: Some(format!("{} {{branch}}", script.display())),
            ..Default::default()
        };
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, "race pr", Some(contract(true, true, true)));
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let repo_hook = repo.clone();
        let runner =
            runner(dir.path(), config, allow_all()).with_pre_step_hook(Arc::new(move |step| {
                if step == CompletionStep::Pr {
                    std::fs::write(repo_hook.join("feature.txt"), "injected after push\n").unwrap();
                }
            }));
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "race pr"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Invalidated),
            "{report:?}"
        );
        assert!(
            !marker.exists(),
            "the PR command must not run against a moved root"
        );
        assert!(report.pr_url.is_none());
        let bare = dir.path().join("remote.git");
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(
            git_output(&bare, &["rev-parse", "main"]),
            head,
            "the push that ran before the edit stays landed"
        );
        assert_task_stays_verifying_and_steps_invalidated(&h, task_id);
    }

    #[tokio::test]
    async fn crash_after_push_before_status_row_is_idempotent_via_remote_tracking() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let bare = add_bare_remote(dir.path(), &repo);
        std::fs::write(repo.join("work.txt"), "landed before the crash\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "faktor: land the work"]);
        // The commit+push happened, but the durable step row never landed
        // (the "crash"). The runner must detect the pushed HEAD through the
        // remote-tracking ref and record Succeeded WITHOUT pushing again.
        git(&repo, &["push", "-q", "origin", "main"]);
        let (h, task_id, tree) = session_bound_to_root(
            &m,
            &repo,
            "crash after push",
            Some(contract(false, true, false)),
        );
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "crash after push"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Push),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Push)
            .unwrap()
            .detail;
        assert!(detail.contains("idempotent replay"), "{detail}");
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        assert_eq!(git_output(&bare, &["rev-parse", "main"]), head);
        let rows = step_rows(&h, task_id);
        assert_eq!(
            rows.len(),
            1,
            "one durable row, no duplicate push: {rows:?}"
        );
        assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
        assert_eq!(
            h.completion_contract_gate(task_id).unwrap(),
            CompletionContractGate::Satisfied
        );
    }

    /// Wave-0 commit race: a human edit injected AFTER the proof
    /// revalidation and BEFORE the tree construction is a typed refusal with
    /// nothing committed — the tree comes from the verified manifest, so a
    /// diverged worktree can never be committed, and the race window between
    /// `add` and `commit` does not exist at all.
    #[tokio::test]
    async fn edit_between_revalidation_and_tree_build_is_refused_with_nothing_committed() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, "commit race", Some(contract(true, false, false)));
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let head_before = git_output(&repo, &["rev-parse", "HEAD"]);
        let hook_repo = repo.clone();
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
            .with_commit_tree_hook(Arc::new(move || {
                std::fs::write(hook_repo.join("README.md"), "human edit mid-add\n").unwrap();
            }));
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "commit race"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Commit),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Commit)
            .unwrap()
            .detail;
        assert!(detail.contains("divergence"), "{detail}");
        assert_eq!(
            git_output(&repo, &["rev-parse", "HEAD"]),
            head_before,
            "nothing was committed"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("README.md")).unwrap(),
            "human edit mid-add\n",
            "the human edit is preserved, never overwritten by a commit"
        );
    }

    /// Wave-0 PR binding: an artifact whose recorded remote head no longer
    /// matches the LIVE remote head refuses the PR step before any provider
    /// call (no PR on top of a moved commit).
    #[tokio::test]
    async fn pr_refuses_when_the_recorded_remote_head_moved() {
        let (dir, m) = manager();
        let repo = init_repo(dir.path());
        let bare = add_bare_remote(dir.path(), &repo);
        let (h, task_id, tree) = session_bound_to_root(
            &m,
            &repo,
            "pr remote drift",
            Some(contract(false, false, true)),
        );
        let (revision, _) = h.completion_contract(task_id).unwrap().unwrap();
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        // The push step published this exact commit earlier: record the
        // artifact with the encoded remote ref.
        let manifest = crate::runtime::merge::canonical_manifest_rows(&repo, MAX_BASE_ENTRIES)
            .unwrap()
            .into_iter()
            .map(|(path, state)| VerifiedManifestEntry {
                path: path.to_string_lossy().into_owned(),
                state,
            })
            .collect();
        let artifact = VerifiedGitArtifact {
            task_id: task_id.raw(),
            revision: revision.raw(),
            verification_record: proof.raw(),
            verified_root_digest: tree.clone(),
            verified_manifest: manifest,
            git_tree_oid: Some(git_output(&repo, &["rev-parse", "HEAD^{tree}"])),
            commit_oid: Some(head.clone()),
            local_ref: Some("refs/heads/main".into()),
            remote_ref: Some(format!("origin:refs/heads/main@{head}")),
            updated_ms: h.now_ms(),
        };
        h.ledger_verified_git_artifact_set(&artifact).unwrap();
        // A human/other runtime moves the remote ref to a different commit.
        std::fs::write(repo.join("other.txt"), "moved on\n").unwrap();
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "faktor: other"]);
        let moved = git_output(&repo, &["rev-parse", "HEAD"]);
        assert_ne!(moved, head);
        git(
            &repo,
            &[
                "push",
                "-q",
                "--force",
                "origin",
                &format!("{moved}:refs/heads/main"),
            ],
        );
        assert_eq!(git_output(&bare, &["rev-parse", "main"]), moved);
        // Keep the local HEAD on the verified commit: only the REMOTE moved.
        git(&repo, &["reset", "-q", "--hard", &head]);
        assert_eq!(git_output(&repo, &["rev-parse", "HEAD"]), head);
        let provider = scm_fake::FakeScmProvider::new();
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all())
            .with_scm_provider(provider.clone());
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "pr remote drift"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Pr)
            .unwrap()
            .detail;
        assert!(detail.contains("live remote head"), "{detail}");
        assert_eq!(
            provider.pr_calls(),
            0,
            "the provider must never be called for a moved remote"
        );
    }

    // --------------------------- durable external-operation identity (B)

    /// A repo whose `origin` names a GitHub repository but has no real
    /// remote: the typed path never touches the network (the provider seam
    /// does), only the push/idempotency checks read refs.
    fn init_repo_with_github_remote(root: &Path) -> std::path::PathBuf {
        let repo = init_repo(root);
        git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        repo
    }

    fn native_pr_fixture(
        dir: &Path,
        goal: &str,
    ) -> (Arc<SessionManager>, SessionHandle, TaskId, String, PathBuf) {
        let m = SessionManager::open(dir.join("native-pr-store"), dir.join("native-pr-cas"), true)
            .unwrap();
        let repo = init_repo_with_github_remote(dir);
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, goal, Some(contract(false, false, true)));
        drive_to_verifying(&h, task_id);
        (m, h, task_id, tree, repo)
    }

    fn native_pr_runner(
        dir: &Path,
        provider: Arc<scm_fake::FakeScmProvider>,
    ) -> CompletionStepRunner {
        runner(dir, CompletionStepsConfig::default(), allow_all()).with_scm_provider(provider)
    }

    fn pr_operation_rows(h: &SessionHandle) -> Vec<faktor_session::ledger::ExternalOperationRow> {
        h.ledger_external_operations().unwrap()
    }

    fn contract_revision(h: &SessionHandle, task_id: TaskId) -> u64 {
        h.completion_contract(task_id)
            .unwrap()
            .expect("contract")
            .0
            .raw()
    }

    /// The create path journals the EXACT operation identity BEFORE the
    /// remote call and the remote object identity after it: two rows, the
    /// first `Prepared` (no remote identity), the last `Completed`.
    #[tokio::test]
    async fn native_pr_records_the_operation_before_the_remote_call() {
        let dir = tempfile::tempdir().unwrap();
        let (m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "native pr");
        let provider = scm_fake::FakeScmProvider::new();
        let runner = native_pr_runner(dir.path(), provider.clone());
        let report = runner
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "native pr"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        assert_eq!(provider.pr_calls(), 1);
        assert_eq!(provider.remote_creations(), 1);
        let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
        let ops: Vec<_> = pr_operation_rows(&h)
            .into_iter()
            .filter(|row| row.operation_key == key)
            .collect();
        assert_eq!(ops.len(), 2, "prepared then completed: {ops:?}");
        assert_eq!(
            ops[0].state,
            faktor_session::ledger::ExternalOperationState::Prepared
        );
        assert!(ops[0].remote_object_id.is_none());
        assert_eq!(ops[0].input.head, "main");
        assert_eq!(
            ops[1].state,
            faktor_session::ledger::ExternalOperationState::Completed
        );
        assert!(ops[1].remote_object_id.is_some());
        assert!(ops[1].remote_object_version.is_some());
        assert!(
            ops[1].reconciled_at.is_none(),
            "first create is not a reconciliation"
        );
        let _ = m;
    }

    /// Crash BEFORE the remote call: exactly one `Prepared` row exists and
    /// the provider was never called. The restart reconciles from the
    /// recorded identity and creates the PR exactly once.
    #[tokio::test]
    async fn native_pr_crash_before_remote_call_retries_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "crash before call");
        let provider = scm_fake::FakeScmProvider::new();
        let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
        let crashed = native_pr_runner(dir.path(), provider.clone())
            .with_pr_crash_seam(PrOperationCrashPoint::BeforeRemoteCall);
        let err = crashed
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "crash before call"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, CompletionStepError::InjectedCrash(_)),
            "{err}"
        );
        assert_eq!(provider.pr_calls(), 0, "the remote call never happened");
        let ops: Vec<_> = pr_operation_rows(&h)
            .into_iter()
            .filter(|row| row.operation_key == key)
            .collect();
        assert_eq!(ops.len(), 1);
        assert_eq!(
            ops[0].state,
            faktor_session::ledger::ExternalOperationState::Prepared
        );
        // The crash left NO step status row: the restart re-executes.
        assert!(step_rows(&h, task_id).is_empty());
        let restarted = native_pr_runner(dir.path(), provider.clone());
        let report = restarted
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "crash before call"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        assert_eq!(provider.pr_calls(), 1, "retried exactly once");
        assert_eq!(provider.remote_creations(), 1);
        let ops: Vec<_> = pr_operation_rows(&h)
            .into_iter()
            .filter(|row| row.operation_key == key)
            .collect();
        assert_eq!(ops.len(), 2);
        assert!(ops[1].reconciled_at.is_some(), "the restart reconciled");
    }

    /// Crash AFTER the remote creation but BEFORE the completion row: the
    /// restart reconciles through the marker/head/base lookup, so the
    /// provider creates exactly ONE PR and the step then certifies.
    #[tokio::test]
    async fn native_pr_crash_after_remote_call_reconciles_without_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "crash after call");
        let provider = scm_fake::FakeScmProvider::new();
        let key = native_pr_operation_key(task_id, contract_revision(&h, task_id), "github");
        let crashed = native_pr_runner(dir.path(), provider.clone())
            .with_pr_crash_seam(PrOperationCrashPoint::AfterRemoteCallBeforeRecord);
        let err = crashed
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "crash after call"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, CompletionStepError::InjectedCrash(_)),
            "{err}"
        );
        assert_eq!(provider.remote_creations(), 1, "the PR was created once");
        let ops: Vec<_> = pr_operation_rows(&h)
            .into_iter()
            .filter(|row| row.operation_key == key)
            .collect();
        assert_eq!(ops.len(), 1, "the completion row never landed");
        assert_eq!(
            ops[0].state,
            faktor_session::ledger::ExternalOperationState::Prepared
        );
        assert!(step_rows(&h, task_id).is_empty());
        let restarted = native_pr_runner(dir.path(), provider.clone());
        let report = restarted
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "crash after call"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Succeeded),
            "{report:?}"
        );
        assert_eq!(
            provider.remote_creations(),
            1,
            "reconciliation must NOT create a duplicate PR"
        );
        assert_eq!(provider.pull_request_count(), 1);
        let ops: Vec<_> = pr_operation_rows(&h)
            .into_iter()
            .filter(|row| row.operation_key == key)
            .collect();
        assert_eq!(ops.len(), 2);
        assert!(ops[1].reconciled_at.is_some());
        assert_eq!(ops[1].remote_object_id.as_deref(), Some("pr-1"));
    }

    /// A durable row under the SAME operation key with a DIFFERENT input
    /// identity (a different head) is a typed refusal: the runtime never
    /// retargets an external operation and never touches the provider.
    #[tokio::test]
    async fn native_pr_input_identity_conflict_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "conflict");
        let revision = contract_revision(&h, task_id);
        let key = native_pr_operation_key(task_id, revision, "github");
        let input = ExternalOperationInput {
            organization: "acme".into(),
            repository: "widgets".into(),
            head: "someone-elses-branch".into(),
            base: "main".into(),
            marker: native_pr_marker(task_id, revision),
        };
        let conflicting = ExternalOperationRow {
            id: ExternalOperationRow::content_id(&key, "github", SCM_PULL_REQUEST_KIND, &input),
            operation_key: key.clone(),
            provider: "github".into(),
            kind: SCM_PULL_REQUEST_KIND.into(),
            input,
            state: faktor_session::ledger::ExternalOperationState::Prepared,
            remote_object_id: None,
            remote_object_version: None,
            started_at: h.now_ms(),
            reconciled_at: None,
        };
        h.ledger_external_operation_set(&conflicting).unwrap();
        let provider = scm_fake::FakeScmProvider::new();
        let runner = native_pr_runner(dir.path(), provider.clone());
        let report = runner
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "conflict"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Pr)
            .unwrap()
            .detail;
        assert!(
            detail.contains("external_operation_input_identity_conflict"),
            "{detail}"
        );
        assert_eq!(provider.pr_calls(), 0, "no remote call on a conflict");
        assert_eq!(provider.branch_calls(), 0);
    }

    /// A recorded remote-object VERSION that the provider no longer reports
    /// is a typed refusal: the run never silently certifies a moved object.
    #[tokio::test]
    async fn native_pr_remote_object_version_mismatch_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "version mismatch");
        let revision = contract_revision(&h, task_id);
        let key = native_pr_operation_key(task_id, revision, "github");
        let input = ExternalOperationInput {
            organization: "acme".into(),
            repository: "widgets".into(),
            head: "main".into(),
            base: "main".into(),
            marker: native_pr_marker(task_id, revision),
        };
        let completed = ExternalOperationRow {
            id: ExternalOperationRow::content_id(&key, "github", SCM_PULL_REQUEST_KIND, &input),
            operation_key: key.clone(),
            provider: "github".into(),
            kind: SCM_PULL_REQUEST_KIND.into(),
            input,
            state: faktor_session::ledger::ExternalOperationState::Completed,
            remote_object_id: Some("pr-1".into()),
            // The recorded version; the provider has moved on.
            remote_object_version: Some("v1".into()),
            started_at: h.now_ms(),
            reconciled_at: None,
        };
        h.ledger_external_operation_set(&completed).unwrap();
        let provider = scm_fake::FakeScmProvider::new();
        provider.force_pull_request_version("moved-v9");
        let runner = native_pr_runner(dir.path(), provider.clone());
        let report = runner
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "version mismatch"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Pr)
            .unwrap()
            .detail;
        assert!(
            detail.contains("external_operation_remote_object_mismatch"),
            "{detail}"
        );
        assert!(detail.contains("moved-v9"), "{detail}");
    }

    /// A branch that moved away from the exact verified head is a typed
    /// refusal BEFORE any PR operation is journaled.
    #[tokio::test]
    async fn native_pr_branch_version_drift_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "branch drift");
        let provider = scm_fake::FakeScmProvider::new();
        provider.force_branch_version("someone-elses-sha");
        let runner = native_pr_runner(dir.path(), provider.clone());
        let report = runner
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "branch drift"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Pr)
            .unwrap()
            .detail;
        assert!(
            detail.contains("external_operation_remote_object_mismatch"),
            "{detail}"
        );
        assert_eq!(provider.pr_calls(), 0, "no PR operation after branch drift");
        assert!(pr_operation_rows(&h).is_empty());
    }

    /// The generic `pr_program` expert command implements NO typed
    /// reconciliation protocol: on the production proof path it is a typed
    /// terminal refusal, never a certification — and it never even runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn pr_program_without_reconciliation_cannot_certify_the_native_pr_step() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo_with_github_remote(dir.path());
        let marker = dir.path().join("pr-program-ran.marker");
        let script = fake_pr_script(
            dir.path(),
            &format!(
                "echo ran > {}\necho https://example.test/pr/1",
                marker.display()
            ),
        );
        let config = CompletionStepsConfig {
            pr_program: Some(script.display().to_string()),
            pr_args: vec!["{branch}".into()],
            ..Default::default()
        };
        let m = SessionManager::open(
            dir.path().join("expert-store"),
            dir.path().join("expert-cas"),
            true,
        )
        .unwrap();
        let (h, task_id, tree) =
            session_bound_to_root(&m, &repo, "expert pr", Some(contract(false, false, true)));
        drive_to_verifying(&h, task_id);
        let proof = passing_record_with_tree(&h, task_id, &tree);
        let runner = runner(dir.path(), config, allow_all());
        let report = runner
            .run_completion_steps(&h, task_id, proof, &ctx(&repo, "expert pr"))
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Failed),
            "{report:?}"
        );
        let detail = &report
            .records
            .iter()
            .find(|r| r.step == CompletionStep::Pr)
            .unwrap()
            .detail;
        assert!(
            detail.contains(PR_REQUIRES_TYPED_RECONCILIATION),
            "{detail}"
        );
        assert!(!marker.exists(), "the expert command must not run");
        assert!(pr_operation_rows(&h)
            .iter()
            .all(|row| row.kind != SCM_PULL_REQUEST_KIND));
        // The gate refuses terminally: the native PR step is never satisfied.
        assert!(matches!(
            h.completion_contract_gate(task_id).unwrap(),
            CompletionContractGate::Refused(TaskError::CompletionStepFailed { .. })
        ));
    }

    /// An UNCONFIGURED PR on the production path stays the historic
    /// retryable `Skipped` (no provider, no command => nothing to certify).
    #[tokio::test]
    async fn unconfigured_native_pr_is_still_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (_m, h, task_id, tree, repo) = native_pr_fixture(dir.path(), "no pr config");
        let runner = runner(dir.path(), CompletionStepsConfig::default(), allow_all());
        let report = runner
            .run_completion_steps(
                &h,
                task_id,
                passing_record_with_tree(&h, task_id, &tree),
                &ctx(&repo, "no pr config"),
            )
            .await
            .unwrap();
        assert_eq!(
            report.outcome_of(CompletionStep::Pr),
            Some(CompletionStepOutcome::Skipped),
            "{report:?}"
        );
        assert!(pr_operation_rows(&h).is_empty());
    }

    /// The repository parser is exact: only a two-segment GitHub reference
    /// is accepted, and the installation identity rides the parsed value.
    #[test]
    fn scm_repository_parsing_is_exact() {
        let parsed = parse_scm_repository("https://github.com/acme/widgets.git", Some("42".into()))
            .expect("https url");
        assert_eq!(parsed.organization, "acme");
        assert_eq!(parsed.repository, "widgets");
        assert_eq!(parsed.installation_id.as_deref(), Some("42"));
        let parsed = parse_scm_repository("git@github.com:acme/widgets.git", None).expect("scp");
        assert_eq!(parsed.organization, "acme");
        assert_eq!(parsed.repository, "widgets");
        for rejected in [
            "",
            "https://gitlab.com/acme/widgets.git",
            "https://github.com/acme",
            "https://github.com/acme/widgets/extra",
            "/tmp/remote.git",
            "https://github.com/acme/widgets.git/../../evil",
        ] {
            assert!(
                parse_scm_repository(rejected, None).is_none(),
                "{rejected:?} must be refused"
            );
        }
    }
}
