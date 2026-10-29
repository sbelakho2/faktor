//! `PromptExecutionService` — the ONE product execution entry for ordinary
//! prompts and task starts (audit: "normal chat still bypasses the
//! shadow/TaskExecutor architecture").
//!
//! Every protocol adapter (Native, ACP, VS Code, JetBrains) translates its
//! DTOs into calls on this service:
//!
//! - [`PromptExecutionService::prompt`] — an ordinary chat prompt becomes an
//!   IN-SESSION run through the daemon's [`TaskExecutor`]; a mutating prompt
//!   works in the daemon-owned isolated candidate and only the verified
//!   integration commit writes the user checkout. No adapter ever calls
//!   `AgentRuntime`'s drive entries directly again.
//! - [`PromptExecutionService::start_task`] — an explicit (multi-work-item)
//!   task run goes through the SAME executor; the daemon allocates the
//!   isolated candidate root itself (a client never supplies a filesystem
//!   path).
//!
//! The service is a stateless facade over the daemon's ONE `TaskExecutor`
//! and ONE `SessionManager`; constructing it per call cannot create a second
//! execution authority, and both the HTTP surface and the ACP backend
//! observe the same underlying instances (see the identity spy tests).

use std::sync::{Arc, Mutex};

use faktor_core::id::{OpId, SessionId};
use faktor_orchestrator::runtime::task_executor::{
    MutationMode, TaskExecutor, TaskRunReceipt, TaskRunRequest,
};
use faktor_orchestrator::runtime::ExecError;
use faktor_orchestrator::{WorkItem, WorkKind};
use faktor_session::SessionManager;

use crate::api::AppState;

/// One protocol-neutral ordinary prompt (the translated DTO).
#[derive(Debug, Clone, Default)]
pub struct PromptRequest {
    /// The prompt text (also the run goal).
    pub prompt: String,
    /// The execution semantics of this prompt (protocol-level `ask` vs
    /// `task`, audit P0-ASK). `Task` — the Default — is today's
    /// engineering-task behavior for every older client (byte-compatible,
    /// no silent change). `Ask` admits a CONVERSATIONAL turn through the
    /// SAME executor/permissions/cancellation/recovery/ownership
    /// infrastructure: no implementation work item, no shadow copy, no
    /// engineering contract, no verification/completion gate, no
    /// task-scoped budget/criteria seam (a cap on an ask is a typed
    /// refusal, never a silent drop).
    pub execution: faktor_protocol::ExecutionKind,
    /// The client's submission UUID when this prompt is a submission-keyed
    /// native task start (`POST /native/session/{id}/task-runs`): the
    /// executor admits the run durably under that key and replays the
    /// original receipt on a repeated key. `None` (ACP and the plain prompt
    /// route) keeps the unkeyed legacy prompt path.
    pub submission_id: Option<String>,
    /// A reservation the CALLER already claimed durably (audit P1): the
    /// ordinary-prompt route reserves the turn's op id with its
    /// `prompt_admission` claim and threads it here, so the admitted turn
    /// journals exactly the op id the claim names. `None` leaves the
    /// executor's own allocation untouched.
    pub reserved_op_id: Option<OpId>,
    /// The request digest of the caller's admission claim (audit P1),
    /// recorded on the run row so admission recovery can detect a tampered
    /// linkage row. `None` for unkeyed prompts.
    pub admission_digest: Option<String>,
    /// Attached file paths (the SDK prompt vocabulary).
    pub files: Vec<String>,
    /// Durable typed binary/image attachments (`AttachmentId` rows),
    /// SEPARATE from the workspace-relative `files`: CAS bytes addressed by
    /// digest, uploaded and validated before this prompt is submitted.
    pub attachments: Vec<faktor_core::attachment::AttachmentId>,
    /// Per-run model override.
    pub model: Option<String>,
    /// Per-run token cap ridden onto the run's durable task row and enforced
    /// by the SAME budget path as an explicit task start. The ordinary
    /// prompt path creates a task row, so a cap has a durable seam here; it
    /// is never silently dropped.
    pub max_tokens: Option<u64>,
    /// Per-run monetary cap (micro-USD), enforced like `max_tokens`.
    pub max_cost_micro: Option<u64>,
    /// Session-selected turn bound applied to the run's durable task row
    /// (F108). `None` keeps the executor default.
    pub max_turns: Option<u32>,
    /// Acceptance criteria ridden onto the run's durable task row.
    pub criteria: Vec<String>,
    /// The run's mutation policy. Legacy vocabulary only: the sole
    /// decodable value is `shadow` (a `direct_compat` value is a strict
    /// decode error naming the removal) and the execution path ignores the
    /// field — every mutating run executes in an isolated candidate.
    pub mutation_mode: Option<MutationMode>,
}

/// The receipt of one accepted ordinary prompt: the in-session run that now
/// carries it (durable linkage row + task row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptReceipt {
    pub run_id: String,
    /// The session's durable turn op id. Present for every local
    /// in-session run; `None` when the worker plane placed the prompt
    /// remotely (the receipt then names the remote job and no local turn
    /// exists).
    pub op_id: Option<OpId>,
    /// The run's durable mode (in-session, orchestrated or remote). The
    /// receipt JSON keeps its historical four fields; adapters use this to
    /// project the remote state without inventing a local turn.
    pub mode: faktor_orchestrator::runtime::task_executor::TaskRunMode,
    pub queued: bool,
    pub accepted: bool,
}

/// What kind of call an observer saw (identity-spy tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptCallKind {
    Prompt,
    StartTask,
}

/// One observed call: the underlying execution authorities are the identity
/// that matters (the facade is stateless).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptCall {
    pub kind: PromptCallKind,
    pub session: SessionId,
    /// `Arc::as_ptr` of the `TaskExecutor` the call executed on.
    pub tasks_ptr: usize,
    /// `Arc::as_ptr` of the `SessionManager` the call executed on.
    pub sessions_ptr: usize,
}

type PromptObserver = Arc<dyn Fn(&PromptCall) + Send + Sync>;

static OBSERVER: Mutex<Option<PromptObserver>> = Mutex::new(None);

/// Install (or clear) the process-wide prompt observer. Test instrumentation;
/// production never installs one, and the lock is a single uncontended
/// mutex on the prompt path.
pub fn set_prompt_observer(observer: Option<PromptObserver>) {
    *OBSERVER.lock().unwrap_or_else(|p| p.into_inner()) = observer;
}

fn observe(service: &PromptExecutionService, kind: PromptCallKind, session: SessionId) {
    let observer = OBSERVER.lock().unwrap_or_else(|p| p.into_inner()).clone();
    if let Some(observer) = observer {
        observer(&PromptCall {
            kind,
            session,
            tasks_ptr: Arc::as_ptr(service.tasks()) as usize,
            sessions_ptr: Arc::as_ptr(service.sessions()) as usize,
        });
    }
}

/// The states that mean "a logical turn is occupying the session machine"
/// (the wait condition every prompt adapter uses before projecting the
/// turn's result). Everything else — Idle, ReadyForNextTurn, Completed,
/// Cancelled, FailedRecoverable/FailedPermanent, NeedsUserInput, Suspended —
/// means the accepted turn has finished (or never started).
pub fn turn_machine_busy(s: faktor_core::state::AgentState) -> bool {
    matches!(
        s,
        faktor_core::state::AgentState::Preparing
            | faktor_core::state::AgentState::BuildingContext
            | faktor_core::state::AgentState::WaitingForModel
            | faktor_core::state::AgentState::Streaming
            | faktor_core::state::AgentState::ToolRequested
            | faktor_core::state::AgentState::WaitingForPermission
            | faktor_core::state::AgentState::ExecutingTool
            | faktor_core::state::AgentState::Validating
            | faktor_core::state::AgentState::UpdatingMemory
    )
}

/// The ONE entry authority for ordinary prompts and task starts.
pub struct PromptExecutionService {
    tasks: Arc<TaskExecutor>,
    sessions: Arc<SessionManager>,
}

impl std::fmt::Debug for PromptExecutionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptExecutionService")
            .finish_non_exhaustive()
    }
}

impl PromptExecutionService {
    pub fn new(tasks: Arc<TaskExecutor>, sessions: Arc<SessionManager>) -> Arc<Self> {
        Arc::new(Self { tasks, sessions })
    }

    /// The daemon's ONE executor authority (identity that all adapters
    /// share).
    pub fn tasks(&self) -> &Arc<TaskExecutor> {
        &self.tasks
    }

    /// The daemon's ONE session authority.
    pub fn sessions(&self) -> &Arc<SessionManager> {
        &self.sessions
    }

    /// The stateless facade over the SERVER deps (constructing it never
    /// builds a second authority).
    pub(crate) fn from_state(state: &AppState) -> Arc<Self> {
        Self::new(state.deps.tasks.clone(), state.deps.session.clone())
    }

    // The daemon-owned candidate root allocation is the executor's own
    // single authority: every orchestrated run allocates its isolated root
    // through [`faktor_orchestrator::runtime::task_executor::CandidateWorkspaceService`]
    // (the DTO never carries a filesystem path).

    /// Run ONE ordinary chat prompt as an in-session task run. The prompt
    /// becomes a single MUTATING work item so the daemon's mutation policy
    /// (shadow by default) gates every write the turn performs.
    /// Build one comparison candidate's durable run request. Kept here (the
    /// executor-facing service) so the native handler has exactly ONE
    /// TaskRunRequest start site, the same static authority as a prompt.
    #[allow(clippy::too_many_arguments)]
    pub fn comparison_candidate_run(
        goal: String,
        submission_id: String,
        model: String,
        prompt: String,
        files: Vec<String>,
        criteria: Vec<String>,
        max_tokens: Option<u64>,
        max_cost_micro: Option<u64>,
        parent_caps: faktor_orchestrator::caps::CapabilitySet,
    ) -> TaskRunRequest {
        let item = WorkItem::new("candidate", prompt, WorkKind::Analysis);
        TaskRunRequest {
            goal,
            work_items: vec![item],
            submission_id: Some(submission_id),
            model: Some(model),
            max_tokens,
            max_cost_micro,
            criteria,
            files,
            parent_caps,
            ..Default::default()
        }
    }

    /// Start one comparison candidate through the SAME executor authority
    /// as a prompt/task start (used by the model-comparison route; the
    /// candidate is an ordinary in-session task with its own session row).
    pub fn start_comparison_candidate(
        &self,
        session: SessionId,
        run: TaskRunRequest,
        max_turns: Option<u32>,
    ) -> Result<TaskRunReceipt, ExecError> {
        self.tasks.start_task_with_turn_cap(session, run, max_turns)
    }

    /// Run ONE ordinary chat prompt as an in-session run through the
    /// daemon's ONE executor entry.
    ///
    /// `execution == Task` (the default): the prompt becomes a single
    /// MUTATING work item so the daemon's mutation policy (shadow by
    /// default) gates every write the turn performs — byte-identical to
    /// every pre-`execution` prompt.
    ///
    /// `execution == Ask` (audit P0-ASK): the SAME entry admits a
    /// CONVERSATIONAL turn as one read-only work item — no implementation
    /// work item, no shadow copy, no engineering task row, no contract, no
    /// verification/completion gate, no task-scoped budget. The task seams
    /// (caps, criteria) are TYPED refusals here: they bind to a durable
    /// task row an ask never creates, and this service never drops them
    /// silently.
    pub async fn prompt(
        &self,
        session: SessionId,
        request: PromptRequest,
    ) -> Result<PromptReceipt, ExecError> {
        if request.prompt.trim().is_empty() {
            return Err(ExecError::InvalidPlan("prompt is empty".into()));
        }
        let ask = request.execution.is_ask();
        if ask
            && (request.max_tokens.is_some()
                || request.max_cost_micro.is_some()
                || request.max_turns.is_some()
                || !request.criteria.is_empty())
        {
            return Err(ExecError::InvalidPlan(
                "an ask (conversational) prompt carries no task budget/turn/criteria seam (they bind to a durable task row an ask never creates); start a task run for budgeted work"
                    .into(),
            ));
        }
        let item = WorkItem::new(
            "main",
            request.prompt.clone(),
            // The ITEM kind is the executor's isolation authority: an ask's
            // read-only Analysis item is never shadowed, while a task's
            // Implementation item keeps the isolated-candidate policy.
            if ask {
                WorkKind::Analysis
            } else {
                WorkKind::Implementation
            },
        );
        let run = TaskRunRequest {
            goal: request.prompt,
            work_items: vec![item],
            execution: request.execution,
            submission_id: request.submission_id,
            reserved_op_id: request.reserved_op_id,
            admission_digest: request.admission_digest,
            model: request.model,
            max_tokens: request.max_tokens,
            max_cost_micro: request.max_cost_micro,
            criteria: request.criteria,
            files: request.files,
            attachments: request.attachments,
            mutation_mode: request.mutation_mode,
            ..Default::default()
        };
        let receipt = if request.max_turns.is_some() {
            self.tasks
                .start_task_with_turn_cap(session, run, request.max_turns)?
        } else {
            self.tasks.start_task(session, run)?
        };
        observe(self, PromptCallKind::Prompt, session);
        Ok(PromptReceipt {
            run_id: receipt.run_id,
            op_id: receipt.op_id,
            mode: receipt.mode,
            queued: receipt.queued,
            accepted: true,
        })
    }

    /// Start ONE explicit task run (the native task-start surface). The
    /// executor allocates the isolated root for multi-item mutating plans
    /// itself.
    /// Start ONE task with the session-selected turn bound (B-09) written
    /// into the durable budget at admission. This is the ONLY executor
    /// entry the server uses.
    pub fn start_task(
        &self,
        session: SessionId,
        request: TaskRunRequest,
        max_turns: Option<u32>,
    ) -> Result<TaskRunReceipt, ExecError> {
        let receipt = self
            .tasks
            .start_task_with_turn_cap(session, request, max_turns)?;
        observe(self, PromptCallKind::StartTask, session);
        Ok(receipt)
    }
}
