//! The daemon's TaskExecutor (audits P0-20/21/23/61/90/91): ONE
//! authoritative entry for native task starts, built over the
//! wave-12 [`super::OrchestratorRuntime`].
//!
//! ```text
//! native task start ──► TaskExecutor::start_task ──┬─ 1 work item ──► the
//!                                                   │    existing session's
//!                                                   │    own drive (agent
//!                                                   │    submit + drive;
//!                                                   │    byte-compatible
//!                                                   │    receipts/events) +
//!                                                   │    durable task row +
//!                                                   │    task-linkage row
//!                                                   └─ ≥ 2 work items ─► plan
//!                                                        row + REAL child
//!                                                        sessions through
//!                                                        execute_task
//!                                                        (one execution at a
//!                                                        time; crashed runs
//!                                                        resume through
//!                                                        resume_run)
//! ```
//!
//! There is NO second execution architecture: a normal single-agent task is
//! the one-simple-work-item case of the SAME executor, and every control
//! (pause/resume/cancel/steer/retry/model/budget) on a child goes through
//! the runtime's durable control queue ([`super::OrchestratorRuntime`]).
//!
//! Crash semantics:
//! - a single-item run IS the daemon's own prompt drive (the agent's
//!   recover/continue paths resume interrupted turns from the durable op
//!   record — never a blind re-run);
//! - a multi-item run leaves durable plan + child rows; a crashed executor
//!   re-attaches through [`TaskExecutor::resume_run`] (wave-12 reattach),
//!   which re-drives every non-terminal child from its durable rows and
//!   applies pending control rows exactly once;
//! - a session whose run still has live (Running/Waiting/Paused) children
//!   refuses a NEW task start with a typed error naming the run — a new run
//!   can never clobber the mirror of a crashed one.
//!
//! Ceilings: one orchestrated (multi-item) execution at a time (the
//! runtime's single-execution architecture is enforced with a typed
//! Conflict), goals bounded to [`crate::MAX_GOAL_CHARS`], work items to
//! the plan validation bounds, linkage rows to the memory-fact cap.

use std::collections::{BTreeSet, HashMap};

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use std::time::{Duration, Instant};

use faktor_agent::AgentRuntime;
use faktor_core::attachment::AttachmentId;
use faktor_core::cancellation::CancellationToken;
use faktor_core::completion::CompletionContract;
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, SessionId, TaskId, TaskRevision, VerificationRecordId, WorktreeId};
use faktor_core::state::{
    command_binding_digest, CandidateProofRef, CheckExecution, CriterionBinding,
    CriterionVerification, EnvironmentFingerprint, NoOpDisposition, TaskState, TaskTransition,
    VerificationStatus,
};
use faktor_session::task::{ProofBasis, ProofBasisCheck, ProofBasisCriterion, ProofReuse};
use faktor_session::{
    CompletionContractGate, SessionManager, ShadowRow, TaskBudget, MAX_TASK_CRITERIA,
    MAX_TASK_CRITERION_BYTES, MAX_TASK_GOAL_BYTES,
};

use super::completion_steps::{
    CompletionStepContext, CompletionStepReport, CompletionStepRunner, CompletionStepsConfig,
    EgressPolicy,
};
use super::shadow::ShadowRoots;
use super::{
    parent_facts, ChildSpec, CrashSeam, ExecConfig, ExecError, OrchestratorRuntime,
    ASSIGNMENT_ROW_KIND, MAX_RUN_ID_CHARS, PLAN_ROW_KIND, REGISTRY_ROW_KIND,
};
use crate::caps::{CapabilityGrant, CapabilitySet, LatticeCap, ScopePattern};
use crate::placement::{PlacementDecision, PlacementSpec, WorkerPlacement};
use crate::{ChildState, OwnershipSpec, TaskPlan, WorkItem, WorkKind, MAX_GOAL_CHARS};

/// Durable row kind of the TaskExecutor task-linkage rows (in-session
/// single-item runs). Deliberately NOT the orchestrator plan/registry kinds:
/// the wave-14 operation graph stays unambiguous for sessions whose
/// in-session runs never spawned children.
pub const TASK_RUN_ROW_KIND: &str = "taskexec_run";
/// Durable row kind of the executor's per-RUN policy facts (the P0 no-op
/// disposition). Keyed by run id; separate from the plan/registry/linkage
/// kinds so every existing reader stays unambiguous.
pub const RUN_POLICY_ROW_KIND: &str = "taskexec_policy";
/// Bound on a linkage-row value (the memory-fact store caps values at 4096
/// bytes; we refuse loudly before the write instead of losing the row).
const MAX_TASK_RUN_ROW_BYTES: usize = 3500;
/// Entry cap of ONE run-base copy (a larger owner tree fails the run loudly).
const MAX_RUN_BASE_ENTRIES: usize = 100_000;
/// Total-byte cap of ONE run-base copy.
const MAX_RUN_BASE_BYTES: u64 = 1024 * 1024 * 1024;
/// Directories the run base never copies (VCS bookkeeping is not content;
/// the root snapshot digest skips exactly these).
const RUN_BASE_SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn"];
/// Bounded retries of the stable run-base copy before a typed
/// [`ExecError::WorkspaceDrift`].
const RUN_BASE_COPY_ATTEMPTS: usize = 3;

/// The canonical tree-manifest digest of one root (`tm1:<64-hex>`; the ONE
/// tree identity defined in `faktor_fs::tree_manifest`). Every run-base,
/// candidate, landing and shadow check goes through this helper — never a
/// second definition of "the same tree" and never the legacy content-only
/// digest.
pub(super) fn root_manifest_digest(root: &std::path::Path) -> Result<String, ExecError> {
    faktor_fs::tree_manifest::tree_manifest_digest(
        root,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .map_err(|e| ExecError::WorkspaceDrift(format!("root snapshot of {}: {e}", root.display())))
}

/// FIX 2: the explicit read distinction, typed inside the executor. A
/// durable row is never collapsed into `None`: absent follows the caller's
/// not-found policy, present-and-valid is used, PRESENT-BUT-CORRUPT refuses
/// as [`Self::CorruptDurableState`], and a failed read refuses as
/// [`Self::StoreFailure`] — never "nothing happened".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurableStateError {
    #[error("corrupt durable state ({what}): {detail}")]
    CorruptDurableState { what: String, detail: String },
    #[error("durable read failed ({what}): {detail}")]
    StoreFailure { what: String, detail: String },
}

impl From<DurableStateError> for ExecError {
    fn from(e: DurableStateError) -> Self {
        // ExecError is shared with every orchestrator surface; the typed
        // refusal keeps its identity in DurableStateError and crosses the
        // boundary with the stable `corrupt durable state` / `durable read
        // failed` prefix.
        ExecError::Internal(format!("{e}"))
    }
}

/// Resolve one [`faktor_session::ledger::DurableRead`]: `Missing` = the
/// caller's not-found policy (`Ok(None)`); valid = use; corrupt or failed =
/// a typed refusal.
fn durable_read_value<T>(
    what: impl Into<String>,
    read: faktor_session::ledger::DurableRead<T>,
) -> Result<Option<T>, DurableStateError> {
    use faktor_session::ledger::DurableRead as R;
    match read {
        R::Missing => Ok(None),
        R::PresentValid(value) => Ok(Some(value)),
        R::PresentMalformed(detail) => Err(DurableStateError::CorruptDurableState {
            what: what.into(),
            detail,
        }),
        R::StoreFailure(detail) => Err(DurableStateError::StoreFailure {
            what: what.into(),
            detail,
        }),
    }
}

/// Classify one session-layer read error: a strict decode/shape failure is
/// corruption, anything else is a store failure. Never "nothing happened".
fn classify_session_read(what: &str, e: faktor_core::Error) -> DurableStateError {
    match e.kind {
        faktor_core::ErrorKind::Malformed => DurableStateError::CorruptDurableState {
            what: what.to_string(),
            detail: e.message,
        },
        _ => DurableStateError::StoreFailure {
            what: what.to_string(),
            detail: e.message,
        },
    }
}

// ------------------------------------------- instruction proof basis (fail-closed)

/// The resolved instruction basis of one proof-basis construction. Exactly
/// TWO success shapes exist; every failure is a typed
/// [`InstructionBasisError`] that PREVENTS proof creation/reuse. The retired
/// `handle.row().ok().and_then(|row| resolver.resolve(..).ok())` collapse
/// turned an unreadable store / hostile tree / resolver failure into "no
/// instructions" — i.e. into a WEAKER basis that silently authorized proof
/// reuse. That path is gone: an unresolved tree is never an absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionBasis {
    /// The session's durable workspace resolves to NO instruction tree (no
    /// durable root): a valid basis carrying no epoch.
    NoApplicableInstructions,
    /// The workspace's live instruction tree resolved and was STABLE across
    /// the double read; the epoch is the resolved rule-tree digest.
    Resolved { epoch: u64 },
}

impl InstructionBasis {
    /// The basis's instruction epoch (`None` only for
    /// [`InstructionBasis::NoApplicableInstructions`]).
    pub fn epoch(self) -> Option<u64> {
        match self {
            InstructionBasis::NoApplicableInstructions => None,
            InstructionBasis::Resolved { epoch } => Some(epoch),
        }
    }
}

/// Typed failure of one instruction-basis resolution. EVERY variant prevents
/// proof creation and proof reuse (the basis is a fail-closed input, never a
/// best-effort one).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InstructionBasisError {
    /// The session row could not be read (store failure / missing row).
    #[error("instruction basis store unavailable ({what}): {detail}")]
    StoreUnavailable { what: String, detail: String },
    /// The session row is present but its workspace relation is corrupt.
    #[error("instruction basis corrupt workspace relation ({what}): {detail}")]
    CorruptWorkspaceRelation { what: String, detail: String },
    /// An authority instruction file is unreadable/oversized.
    #[error("instruction basis file unreadable: {0}")]
    UnreadableInstructionFile(String),
    /// The instruction tree moved between the two reads of one basis
    /// construction: a moving tree can never be a stable proof basis.
    #[error("instruction basis tree is unstable: {0}")]
    UnstableInstructionTree(String),
    /// Any other resolver failure (never collapsed into an absence).
    #[error("instruction resolver internal error: {0}")]
    ResolverInternal(String),
}

/// Classify one resolver error into the typed basis failure.
fn classify_rules_load_error(e: faktor_instructions::RulesLoadError) -> InstructionBasisError {
    use faktor_instructions::RulesLoadError as E;
    match &e {
        E::Oversized(message) | E::Unreadable(message) => {
            InstructionBasisError::UnreadableInstructionFile(message.clone())
        }
        E::EpochMismatch { .. } => InstructionBasisError::ResolverInternal(e.to_string()),
    }
}

/// How one [`TaskRunRequest`] executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskRunMode {
    /// The run drives the EXISTING parent session with the daemon's own
    /// drive path (one simple work item; the session's workspace is the
    /// worktree it already owns).
    InSession,
    /// The run spawned real child sessions through `execute_task`.
    Orchestrated,
    /// The run was placed on a remote worker: one immutable job generation
    /// was minted and leased in the worker plane and NO local execution was
    /// started (the worker plane's durable rows own the attempt).
    /// Only reachable when the worker plane is enabled: with the plane
    /// disabled (the default) this variant is never produced.
    Remote,
}

/// The P0 mutation policy of a MUTATING single-item run. There is exactly
/// ONE policy — `Shadow` (the production default and the only decodable
/// value): a single-item MUTATING run works in a daemon-owned isolated
/// candidate (the [`crate::runtime::shadow`] machinery) and only a
/// conflict-aware verified integration commits the user checkout.
/// Read-only single-item runs and multi-item runs never shadow (they never
/// mutate the owner checkout through the in-session drive).
///
/// The type survives ONLY as the wire/config vocabulary: the legacy
/// `direct_compat` value is a strict decode error naming its removal, and
/// there is no mode value, config key or DTO field that can disable
/// isolation (see [`TaskExecutor::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationMode {
    #[default]
    Shadow,
}

impl<'de> serde::Deserialize<'de> for MutationMode {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct ModeVisitor;
        impl serde::de::Visitor<'_> for ModeVisitor {
            type Value = MutationMode;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("the mutation policy \"shadow\" (mutating runs always execute in an isolated candidate)")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<MutationMode, E> {
                match value {
                    "shadow" => Ok(MutationMode::Shadow),
                    "direct_compat" => Err(E::custom(
                        "mutation_mode \"direct_compat\" was removed: every mutating run executes in an isolated candidate (shadow mutation); there is no direct-owner mode",
                    )),
                    other => Err(E::unknown_variant(other, &["shadow"])),
                }
            }
        }
        de.deserialize_str(ModeVisitor)
    }
}

/// The durable linkage row of ONE in-session (single-item) task run,
/// scoped to the parent session's fact space (kind [`TASK_RUN_ROW_KIND`],
/// key = run id). Orchestrated runs carry their own durable plan/registry
/// rows instead; the linkage row makes an in-session run visible to the
/// native agent listing with the same vocabulary as a plan.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskRunRow {
    pub run_id: String,
    pub session_id: u64,
    pub mode: TaskRunMode,
    /// The task goal (also the single-item prompt).
    pub goal: String,
    pub item_ids: Vec<String>,
    /// The run's immutable attachment set (the SDK `PromptRequest.files`
    /// vocabulary), validated with the ONE rule
    /// ([`super::validate_attachment_files`]) before any durable row and
    /// persisted HERE so a reopened daemon serves the byte-identical set.
    /// Old rows decode with an empty list (field-level serde default): the
    /// attachment-free run stays byte-identical.
    #[serde(default)]
    pub files: Vec<String>,
    /// The run's immutable BINARY attachment set (`AttachmentId` rows,
    /// schema v24) — SEPARATE from `files`: CAS bytes addressed by digest,
    /// never workspace paths. Persisted here with the same additive
    /// serde-default rule so a reopened/re-attached daemon reconstructs the
    /// byte-identical typed set ([`super::validate_attachment_ids`]).
    #[serde(default)]
    pub attachments: Vec<AttachmentId>,
    /// The session's durable turn op id of this run (0 until submitted).
    pub op_id: Option<u64>,
    pub model: Option<String>,
    pub budget_max_tokens: Option<u64>,
    pub created_ms: i64,
}

impl TaskRunRow {
    /// Decode one durable row value (hostile/tampered values are loud).
    pub fn decode(value: &str) -> Result<Self, String> {
        serde_json::from_str(value).map_err(|e| format!("taskexec run row decode: {e}"))
    }
}

/// The durable per-run policy value (kind [`RUN_POLICY_ROW_KIND`], key = run
/// id). Additive: an old row without the field decodes with `None`, and
/// `None` resolves to the mutating-task default (`RequiresCriterionProof`) —
/// never a silent `Allowed`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunPolicyRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    no_op_disposition: Option<NoOpDisposition>,
}

/// Record one orchestrated run's durable policy row BEFORE the run is
/// claimed (the settlement reads it back; a failed write leaves no claimed
/// slot).
fn write_run_policy_row(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    no_op_disposition: Option<NoOpDisposition>,
) -> Result<(), ExecError> {
    let value = serde_json::to_string(&RunPolicyRow { no_op_disposition })
        .map_err(|e| ExecError::Internal(format!("run policy row encode: {e}")))?;
    handle
        .upsert_memory_fact(RUN_POLICY_ROW_KIND, run_id, &value)
        .map_err(|e| ExecError::Internal(format!("run policy row write: {e}")))
}

/// The run's effective no-op disposition from its durable policy row; an
/// absent row (a legacy run) or an absent field resolves to the
/// mutating-task default — never a silent `Allowed`. FIX 2: a failed read
/// and a PRESENT-but-undecodable row are errors (the corrupt row is named),
/// never a silent default.
fn run_no_op_disposition(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
) -> Result<NoOpDisposition, ExecError> {
    let facts = parent_facts(handle)?;
    let Some((_, _, value)) = facts
        .into_iter()
        .find(|(kind, key, _)| kind == RUN_POLICY_ROW_KIND && key == run_id)
    else {
        return Ok(NoOpDisposition::default_for_mutating_task());
    };
    let row: RunPolicyRow = serde_json::from_str(&value).map_err(|e| {
        ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("run policy row of run {run_id}"),
            detail: e.to_string(),
        })
    })?;
    Ok(row
        .no_op_disposition
        .unwrap_or_else(NoOpDisposition::default_for_mutating_task))
}

/// One native task start: a goal plus one or more work items. Dispatch is
/// by item count: one item drives the existing session (single-agent case),
/// two or more spawn real children through the orchestrator runtime.
#[derive(Debug, Clone)]
pub struct TaskRunRequest {
    pub goal: String,
    pub work_items: Vec<WorkItem>,
    /// Model selector used for the single-item drive / child default.
    pub model: Option<String>,
    /// Durable token budget cap applied to the run (task row / children).
    pub max_tokens: Option<u64>,
    /// Durable MONETARY cap (microUSD) applied to the run's task row: the
    /// single-item drive's own row, or the orchestrated run's ROOT task row
    /// under which child budget scopes enroll (children get their own caps
    /// through the child budget-change surface). `None` = no cost cap (the
    /// behavior of every previous wave). 0 is treated as unlimited by the
    /// store ledger, matching the `max_tokens` axis.
    pub max_cost_micro: Option<u64>,
    /// Item ids that complete WITHOUT a spawned child (auto steps of the
    /// plan; every other item spawns a real child session).
    pub auto_items: Vec<String>,
    /// Acceptance criteria of the run (bounded: at most
    /// [`MAX_TASK_CRITERIA`] entries of at most
    /// [`MAX_TASK_CRITERION_BYTES`] bytes each — the same caps the durable
    /// task row enforces; beyond them is a typed Oversized refusal before
    /// anything is written). They land on the session's durable task row
    /// the drive certifies against (single-item runs always seed one;
    /// multi-item runs seed the run's ROOT row when criteria are present).
    pub criteria: Vec<String>,
    /// Tri-state re-goal patch for the run's acceptance criteria (FIX 3):
    /// `None` = continuation (the durable task row keeps its criteria),
    /// `Some(vec![])` = explicitly CLEAR, `Some(items)` = REPLACE. When this
    /// field is `Some` it takes precedence over the legacy `criteria` vector
    /// (whose empty value keeps its historical continuation meaning for
    /// every existing caller that never sets this field).
    pub criteria_patch: Option<Vec<String>>,
    /// The run's mutation policy. Kept for wire compatibility ONLY: the
    /// only decodable value is [`MutationMode::Shadow`] (a `direct_compat`
    /// value is a strict decode error naming the removal), `None` resolves
    /// to it, and the execution path ignores the field entirely — a
    /// mutating run ALWAYS executes in an isolated candidate. See
    /// [`MutationMode`] and [`TaskExecutor::new`].
    pub mutation_mode: Option<MutationMode>,
    /// Files attached to the ordinary prompt (the SDK `PromptRequest.files`
    /// vocabulary). They ride the SAME in-session drive submit as a plain
    /// prompt — bounded by the session layer's own prompt bounds.
    pub files: Vec<String>,
    /// Durable typed binary/image attachments of the run (`AttachmentId`
    /// rows), SEPARATE from `files` (never workspace paths). Structural
    /// bounds/validity are enforced by [`Self::validate`] before any durable
    /// row; durable-row existence is resolved by the session layer at
    /// admission and image delivery is validated against the chosen model's
    /// capabilities at the server DTO. They land on the run's durable task
    /// row and on every child spec; each spawned child inherits the verified
    /// rows so a re-attach reconstructs the identical typed set and request
    /// construction resolves the same bytes.
    pub attachments: Vec<AttachmentId>,
    /// Tri-state attachment patch (FIX 3, same contract as
    /// [`Self::criteria_patch`]): `None` = continuation, `Some(vec![])` =
    /// clear the durable attachment set, `Some(items)` = replace it. When
    /// set it takes precedence over the legacy `attachments` vector.
    pub attachments_patch: Option<Vec<AttachmentId>>,
    /// Capability ceiling of the parent (children get parent ∩ policies).
    pub parent_caps: CapabilitySet,
    pub ceilings: super::Ceilings,
    /// The PR/CI-fix completion contract of this run (P2). `None` (the
    /// default) leaves the completion path byte-identical to every previous
    /// wave: no durable contract row, no gate. `Some(non-default)` is
    /// recorded durably as a `CompletionContractSet` row BEFORE the run's
    /// first model call; `VerifiedComplete` then requires a durable
    /// `Succeeded` step-status row for every requested step.
    pub completion_contract: Option<CompletionContract>,
    /// Root under which isolated child workspaces are created. Empty on the
    /// wire (the DTO never carries a filesystem path): the executor
    /// allocates a daemon-owned candidate root through its
    /// [`CandidateWorkspaceService`] before any durable row. A non-empty
    /// root is the programmatic/test override.
    pub isolated_root: PathBuf,
    /// P0 no-op policy of the run: what an EMPTY aggregate change set may
    /// do. `None` = [`NoOpDisposition::default_for_mutating_task`]
    /// (`RequiresCriterionProof`). Persisted durably with the run BEFORE the
    /// first spawn, so a re-settled run applies the byte-identical policy.
    pub no_op_disposition: Option<NoOpDisposition>,
    /// Deterministic crash seam (adversarial tests only).
    pub crash_seam: Option<CrashSeam>,
}

impl Default for TaskRunRequest {
    fn default() -> Self {
        Self {
            goal: String::new(),
            work_items: Vec::new(),
            model: None,
            max_tokens: None,
            max_cost_micro: None,
            auto_items: Vec::new(),
            criteria: Vec::new(),
            criteria_patch: None,
            mutation_mode: None,
            files: Vec::new(),
            attachments: Vec::new(),
            attachments_patch: None,
            parent_caps: CapabilitySet::new(),
            ceilings: super::Ceilings::default(),
            completion_contract: None,
            isolated_root: PathBuf::new(),
            no_op_disposition: None,
            crash_seam: None,
        }
    }
}

impl TaskRunRequest {
    /// Structural validation (bounded everything): goal non-empty and
    /// within the plan bound, item ids sane, model bounded, ceilings sane.
    /// A single mutating item is legal (it drives the session, which owns
    /// its worktree); a multi-item plan must be a valid [`TaskPlan`].
    pub fn validate(&self) -> Result<(), ExecError> {
        if self.goal.trim().is_empty() {
            return Err(ExecError::InvalidPlan("goal is empty".into()));
        }
        if self.goal.chars().count() > MAX_GOAL_CHARS {
            return Err(ExecError::Oversized(format!(
                "goal exceeds {MAX_GOAL_CHARS} characters"
            )));
        }
        if self.work_items.is_empty() {
            return Err(ExecError::InvalidPlan(
                "a task needs at least one work item".into(),
            ));
        }
        if self.criteria.len() > MAX_TASK_CRITERIA {
            return Err(ExecError::Oversized(format!(
                "{} acceptance criteria exceed MAX_TASK_CRITERIA ({MAX_TASK_CRITERIA})",
                self.criteria.len()
            )));
        }
        // The tri-state patches are THE effective criteria/attachments when
        // set (FIX 3): validate exactly the set that will be applied.
        if let Some(criteria) = &self.criteria_patch {
            if criteria.len() > MAX_TASK_CRITERIA {
                return Err(ExecError::Oversized(format!(
                    "{} patched acceptance criteria exceed MAX_TASK_CRITERIA ({MAX_TASK_CRITERIA})",
                    criteria.len()
                )));
            }
            for c in criteria {
                if c.trim().is_empty() || c.len() > MAX_TASK_CRITERION_BYTES {
                    return Err(ExecError::Oversized(format!(
                        "a patched acceptance criterion of {} bytes exceeds MAX_TASK_CRITERION_BYTES ({MAX_TASK_CRITERION_BYTES}) or is empty",
                        c.len()
                    )));
                }
            }
        }
        // Attachments are part of the run's contract: validated with the ONE
        // shared rule (the same MAX_FILES_PER_PROMPT / MAX_FILE_PATH_BYTES
        // bounds the single-session prompt submission enforces, plus the
        // typed hostile-path refusal) BEFORE anything durable is written.
        super::validate_attachment_files(&self.files)?;
        // Binary attachments are validated with their OWN ONE rule (bounded
        // count and structural id validity) BEFORE anything durable is
        // written; durable-row existence is resolved by the session layer at
        // admission and image delivery is gated on the chosen model's
        // capabilities at the server DTO. The effective (tri-state) set is
        // what gets validated: `Some(vec![])` clears and validates as empty.
        super::validate_attachment_ids(&self.attachments)?;
        if let Some(attachments) = &self.attachments_patch {
            super::validate_attachment_ids(attachments)?;
        }
        for c in &self.criteria {
            if c.trim().is_empty() || c.len() > MAX_TASK_CRITERION_BYTES {
                return Err(ExecError::Oversized(format!(
                    "an acceptance criterion of {} bytes exceeds MAX_TASK_CRITERION_BYTES ({MAX_TASK_CRITERION_BYTES}) or is empty",
                    c.len()
                )));
            }
        }
        if let Some(m) = &self.model {
            if m.is_empty() || m.chars().count() > 128 {
                return Err(ExecError::Oversized(
                    "model selector must be 1..=128 characters".into(),
                ));
            }
        }
        self.ceilings.validate().map_err(ExecError::InvalidPlan)?;
        // (audits 7/8/21/22, work-entry unification) Plan validation reads
        // the ITEM's own ownership — the only authority there is. A mutating
        // item whose spec is still NoWrites (a decoded legacy row, a
        // hand-built DTO) is InvalidPlan here; nothing defaults a write
        // authority onto it. Legacy plan-global conversion happens exactly
        // once, at the DTO/durability boundary, never here.
        let plan = self.plan_for_validation();
        plan.validate()
            .map_err(|errs| ExecError::InvalidPlan(errs.join("; ")))?;
        for id in &self.auto_items {
            if !self.work_items.iter().any(|w| &w.id == id) {
                return Err(ExecError::InvalidPlan(format!(
                    "auto item {id:?} does not name a work item"
                )));
            }
        }
        Ok(())
    }

    /// The validation-shaped plan over the request's work items; ownership
    /// rides each item ([`WorkItem::ownership`]), never the request or the
    /// plan. An empty `isolated_root` is legal: the executor allocates a
    /// daemon-owned candidate root through its [`CandidateWorkspaceService`]
    /// before any durable row.
    pub fn plan_for_validation(&self) -> TaskPlan {
        TaskPlan {
            goal: self.goal.clone(),
            non_goals: Vec::new(),
            constraints: Vec::new(),
            work_items: self.work_items.clone(),
        }
    }

    /// The effective tri-state acceptance-criteria patch of this request
    /// (FIX 3): `None` = continuation, `Some(vec![])` = clear,
    /// `Some(items)` = replace. The dedicated patch field wins when set; the
    /// legacy `criteria` vector maps to `Some(items)` when non-empty and
    /// `None` when empty (its historical continuation semantics).
    pub fn effective_criteria_patch(&self) -> Option<Vec<String>> {
        match &self.criteria_patch {
            Some(items) => Some(items.clone()),
            None if self.criteria.is_empty() => None,
            None => Some(self.criteria.clone()),
        }
    }

    /// The effective tri-state attachment patch (FIX 3): the dedicated
    /// patch field wins; the legacy `attachments` vector maps to
    /// `Some(items)` when non-empty and `None` when empty.
    pub fn effective_attachments_patch(&self) -> Option<Vec<AttachmentId>> {
        match &self.attachments_patch {
            Some(items) => Some(items.clone()),
            None if self.attachments.is_empty() => None,
            None => Some(self.attachments.clone()),
        }
    }
}

/// The receipt of one accepted task start. Single-item receipts carry the
/// REAL session op id + queued state of the submitted prompt (byte
/// compatible with the daemon's prompt path); orchestrated receipts carry
/// the durable run id (children appear under it in the operation graph).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRunReceipt {
    pub run_id: String,
    pub mode: TaskRunMode,
    pub op_id: Option<OpId>,
    pub queued: bool,
}

/// Which durable run one [`TaskExecutor::settle_run`] pass settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSettlement {
    /// The single-item in-session run of one session. Its drive already
    /// performed the deterministic verification AND drove the completion
    /// gate (the exact single-agent ordering, preserved); settlement
    /// executes the accepted contract's requested steps (idempotently) and
    /// then finalizes the run's shadow. `run_id` is carried for reporting.
    InSession { parent: SessionId, run_id: String },
    /// A multi-item orchestrated run: settlement folds the run's children
    /// into the ROOT task's aggregate deterministic verification (the
    /// tournament-style derived check set over the parent's criteria, never
    /// per candidate), executes the contract's steps, drives the completion
    /// gate (`complete_verified_task`) and leaves every child candidate root
    /// untouched (explicit-merge proposal only).
    Orchestrated { parent: SessionId, run_id: String },
}

/// The durable outcome of one settlement pass. Replaying a settled run is
/// an idempotent no-op that returns the same truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementOutcome {
    /// The settled run id.
    pub run_id: String,
    /// `true` for the orchestrated path.
    pub orchestrated: bool,
    /// The run's work is fully done: every durable SPAWN item of the run has
    /// a `Done` child (orchestrated), or the drive's task row is terminal
    /// (in-session).
    pub complete: bool,
    /// The verification driving THIS settlement says `passed`.
    pub verified: bool,
    /// The run's ROOT task row holds `VerifiedComplete`.
    pub completed: bool,
    /// The aggregate root verification record the orchestrated gate used.
    pub verification: Option<VerificationRecordId>,
    /// The completion-step report of this pass, when a contract ran.
    pub steps: Option<CompletionStepReport>,
    /// Child roots an EXPLICIT merge may consume (orchestrated only).
    /// Settlement NEVER merges or commits them.
    pub merge_proposals: Vec<String>,
    /// The shadow finalize outcome (in-session shadowed runs; `None` when
    /// the run carries no live shadow).
    pub finalize: Option<ShadowFinalize>,
}

/// One staged child change set of a prepared run integration: the child,
/// its isolated root, the STAGED candidate and the content digest of that
/// root at staging time.
#[derive(Debug, Clone)]
pub struct PreparedChildChangeSet {
    pub child_id: String,
    pub child_root: PathBuf,
    pub change_set: crate::runtime::merge::ChangeSet,
    pub candidate_root_hash: String,
}

/// The prepared integration candidate of one orchestrated run — composed
/// from the IMMUTABLE run base and verified BEFORE any owner mutation. The
/// verifier consumes `candidate_root`/`candidate_snapshot`; only the landing
/// phase may touch `owner_root`.
#[derive(Debug, Clone)]
pub struct PreparedRunIntegration {
    pub run_id: String,
    pub task_id: TaskId,
    pub owner_root: PathBuf,
    pub base_root: PathBuf,
    pub candidate_root: PathBuf,
    /// The run base's snapshot digest (the owner equality anchor).
    pub base_snapshot: String,
    /// The COMPOSED candidate root's snapshot digest (what verification
    /// binds and what the landed owner must equal).
    pub candidate_snapshot: String,
    /// The full sorted union of staged change-set paths.
    pub changed: Vec<String>,
    pub sources: Vec<faktor_session::IntegrationSourceRow>,
    pub sources_digest: String,
    pub staged: Vec<PreparedChildChangeSet>,
}

/// A PASSING verification of one [`PreparedRunIntegration`]: the proof the
/// landing phase consumes (typed refusal when the verifier cannot run).
///
/// The type is constructible ONLY from the verdict
/// [`compose_root_verification_status`] derives from the typed criterion
/// verdicts: green checks never authorize landing over a failed or
/// unavailable required criterion. The construction module is private, so no
/// caller — in this crate or another — can assemble one from fields or mint
/// one from an independently supplied status.
pub use verified_integration::VerifiedRunIntegration;

/// The private construction module of [`VerifiedRunIntegration`]: fields are
/// private HERE (not merely to the parent), so a struct literal is impossible
/// outside this module and the only path to a value is the validating
/// constructor that recomposes the verdict from its typed evidence.
mod verified_integration {
    use super::{
        compose_no_op_root_verification_status, compose_root_verification_status,
        PreparedRunIntegration,
    };
    use faktor_core::id::VerificationRecordId;
    use faktor_core::state::{CheckExecution, CriterionVerification, VerificationStatus};

    #[derive(Debug, Clone)]
    pub struct VerifiedRunIntegration {
        prepared: PreparedRunIntegration,
        record: VerificationRecordId,
        composed_status: VerificationStatus,
        /// The proof-basis digest the verification record was created or
        /// reused under. Rides the landing into the durable integration
        /// record so the landed snapshot is bound to EXACTLY the basis that
        /// certified it.
        proof_basis_digest: String,
    }

    impl VerifiedRunIntegration {
        /// The ONLY constructor. It recomposes the verdict from the SAME
        /// typed evidence and refuses (`None`) unless the recomposed verdict
        /// equals `composed` AND is `Passed`: the persisted status and the
        /// landing proof can never disagree, and a non-passing composition
        /// can never mint a proof at all. `no_op` selects the stricter
        /// all-criteria-pass rule of an empty aggregate change set.
        pub(super) fn from_composed_verdict(
            prepared: PreparedRunIntegration,
            record: VerificationRecordId,
            checks: &[CheckExecution],
            criteria: &[CriterionVerification],
            no_op: bool,
            composed: VerificationStatus,
            proof_basis_digest: String,
        ) -> Option<Self> {
            let derived = if no_op {
                compose_no_op_root_verification_status(checks, criteria)
            } else {
                compose_root_verification_status(checks, criteria)
            };
            if derived != composed || composed != VerificationStatus::Passed {
                return None;
            }
            Some(Self {
                prepared,
                record,
                composed_status: composed,
                proof_basis_digest,
            })
        }

        /// The prepared candidate this proof authorizes landing of.
        pub(super) fn prepared(&self) -> &PreparedRunIntegration {
            &self.prepared
        }

        /// The durable verification record id of the composed verdict.
        pub(super) fn record(&self) -> VerificationRecordId {
            self.record
        }

        /// The composed verdict every landing decision is made from.
        pub(super) fn composed_status(&self) -> VerificationStatus {
            self.composed_status
        }

        /// The proof-basis digest the record was created/reused under.
        pub(super) fn proof_basis_digest(&self) -> &str {
            &self.proof_basis_digest
        }
    }
}

/// The materialized landing of one verified integration: owner now holds the
/// candidate content, with a FRESH whole-root digest equal to the verified
/// candidate snapshot.
#[derive(Debug, Clone)]
pub struct LandedRunIntegration {
    pub prepared: PreparedRunIntegration,
    pub record: VerificationRecordId,
    /// The fresh whole-root digest taken AFTER the landing.
    pub landed_snapshot: String,
}

/// One active orchestrated execution of the executor (audits 7/8/21/22:
/// runs are keyed by RUN ID and indexed per PARENT SESSION — the executor
/// keeps ONE run per parent session; runs of different sessions proceed
/// concurrently through the runtime's run-scoped mirrors. Global limits
/// stay in the scheduler ceilings / provider limits / live-child ceiling,
/// never in a global execution slot).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveRun {
    parent: SessionId,
    run_id: String,
}

/// The typed refusal of a POISONED executor authority (FIX 2): a writer
/// panicked while holding one of the executor's internal locks, so the
/// authority can no longer be trusted. Every work path that must consult it
/// refuses — it is never treated as "no active run" (which would free a
/// slot that may still be occupied) and never as "an active run" (which
/// would silently skip settlement).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("poisoned authority {authority}: {detail}")]
pub struct PoisonedAuthority {
    /// Which authority is poisoned.
    pub authority: &'static str,
    /// Why it is poisoned (the panic poison's message).
    pub detail: String,
}

impl PoisonedAuthority {
    /// The poisoned ACTIVE-RUN lock (the run-claim authority).
    pub fn active_run_lock(detail: impl std::fmt::Display) -> Self {
        Self {
            authority: "active-run lock",
            detail: format!(
                "a writer panicked while holding it ({detail}); refusing the work rather than \
                 guessing the active-run set"
            ),
        }
    }

    /// The poisoned COMPLETION-STEP policy lock: the validated
    /// `[completion]` execution policy can no longer be trusted, so a policy
    /// mutation refuses rather than half-applying a commit/push/PR config.
    pub fn completion_step_lock(detail: impl std::fmt::Display) -> Self {
        Self {
            authority: "completion-step policy lock",
            detail: format!(
                "a writer panicked while holding it ({detail}); refusing the configuration \
                 change rather than half-applying a commit/push/PR policy"
            ),
        }
    }
}

impl From<PoisonedAuthority> for ExecError {
    fn from(e: PoisonedAuthority) -> Self {
        // ExecError is a frozen shared type; the typed refusal keeps its own
        // identity in PoisonedAuthority and crosses the ExecError boundary
        // with a stable `poisoned authority` prefix.
        ExecError::Internal(format!("poisoned authority: {e}"))
    }
}

/// The daemon-owned candidate/isolated root allocator (audits 7/8/21/22 +
/// P1 native mutating multi-agent): ONE authority per executor, rooted
/// under the daemon's data directory (the directory of the session store).
/// A client NEVER supplies a filesystem path — the dto carries none — the
/// daemon allocates `root/s<session>/<run>` and hands the path to the
/// runtime's isolated child workspaces.
pub struct CandidateWorkspaceService {
    root: PathBuf,
}

impl std::fmt::Debug for CandidateWorkspaceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandidateWorkspaceService")
            .field("root", &self.root)
            .finish()
    }
}

impl CandidateWorkspaceService {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self { root })
    }

    /// The base under which every run's candidate root is allocated.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Allocate (and create) the daemon-owned isolated root of ONE run:
    /// `<root>/s<session>/<run>`. The run id is validated with the same
    /// charset/bound the durable rows enforce; a hostile id never escapes
    /// the root. Idempotent for the same (session, run).
    pub fn allocate(&self, session: SessionId, run_id: &str) -> Result<PathBuf, ExecError> {
        if run_id.is_empty()
            || run_id.len() > MAX_RUN_ID_CHARS
            || !run_id.is_ascii()
            || run_id.contains('/')
            || run_id.contains('\\')
            || run_id.chars().any(|c| c.is_control())
        {
            return Err(ExecError::Oversized(format!(
                "candidate run id must be 1..={MAX_RUN_ID_CHARS} ASCII characters without '/' or '\\\\'"
            )));
        }
        let dir = self.root.join(format!("s{}", session.raw())).join(run_id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| ExecError::Internal(format!("candidate run root {dir:?}: {e}")))?;
        Ok(dir)
    }
}

/// The authoritative task executor of the daemon graph (audits P0-20/21):
/// [`TaskExecutor::start_task`] dispatches single-item runs to the existing
/// session's own drive and multi-item runs to the orchestrator runtime's
/// real child sessions.
///
/// P0-48: every MUTATING single-item run executes inside a daemon-owned
/// isolated candidate (the [`ShadowRoots`] machinery) and only a
/// conflict-aware verified integration commits the user checkout (see
/// [`TaskExecutor::finalize_shadow_run`]); read-only runs need none. There
/// is NO mode value, config key or DTO field that can disable isolation:
/// the production constructor ([`TaskExecutor::new`]) REQUIRES the shadow
/// service, and the only owner-direct executor is the `#[cfg(test)]`
/// developer seam [`TaskExecutor::new_owner_direct_for_test_harness`],
/// which is compiled out of release builds.
pub struct TaskExecutor {
    orchestrator: Arc<OrchestratorRuntime>,
    session: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    /// Active orchestrated runs by run id (audits 7/8/21/22): ONE run per
    /// parent session (per-session sequential), while runs of different
    /// parent sessions run concurrently through the runtime's run-scoped
    /// mirrors.
    active: Mutex<HashMap<String, ActiveRun>>,
    /// The daemon's shadow service. Production ALWAYS carries it (the
    /// production constructor takes an `Arc<ShadowRoots>`, not an option);
    /// `None` exists only for the test seam
    /// [`TaskExecutor::new_owner_direct_for_test_harness`] — the low-level
    /// suites that genuinely drive the owner checkout directly.
    shadows: Option<Arc<ShadowRoots>>,
    /// The ONE candidate-root allocator: every orchestrated run's isolated
    /// root is allocated here, never supplied by a client.
    run_roots: Arc<CandidateWorkspaceService>,
    /// P2 completion-step wiring: the strict execution config plus the
    /// lazily built runner (over the agent's own supervisor + sandbox
    /// egress policy). `None` runner = no completion step is ever invoked;
    /// an uncontracted run never touches this field beyond `None`.
    completion_steps: Mutex<CompletionStepsWiring>,
    /// Deterministic settlement crash seam (adversarial tests only): the
    /// NEXT settlement (or run-base creation) returns
    /// [`ExecError::InjectedCrashSeam`] at the FIRST matching boundary,
    /// leaving every durable row exactly as a real crash would. One-shot.
    settlement_seam: Mutex<Option<(CrashSeam, bool)>>,
    /// The additive worker-plane placement seam. Default
    /// [`WorkerPlacement::disabled`]: every placement decision is local and
    /// this executor is byte-identical to the pre-worker-plane executor.
    /// Enabled by the daemon when the `[workers]` section is configured.
    placement: Mutex<WorkerPlacement>,
}

/// The completion-step wiring of one executor: the configured template
/// values and the cached runner (rebuilt when the config changes).
#[derive(Default)]
struct CompletionStepsWiring {
    config: CompletionStepsConfig,
    runner: Option<Arc<CompletionStepRunner>>,
}

impl std::fmt::Debug for TaskExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskExecutor").finish_non_exhaustive()
    }
}

/// What one shadowed run's terminal finalize did (P0-48).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowFinalizeAction {
    /// Verified-complete + clean integration: the user checkout holds the
    /// shadow's content; the shadow directory is gone.
    Integrated,
    /// Verified-complete + integration CONFLICTS: nothing of the conflicted
    /// run landed in the user checkout; the shadow is retained (row
    /// `IntegrationBlocked`) with the conflict list recorded durably.
    IntegrationBlocked,
    /// The run failed/was cancelled: the shadow was discarded.
    Discarded,
    /// The run is not terminal yet (e.g. verification still pending or the
    /// task needs another drive): the shadow stays and nothing was applied.
    Retained,
}

/// The durable outcome summary of one shadowed-run finalize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowFinalize {
    pub action: ShadowFinalizeAction,
    pub merged: Vec<std::path::PathBuf>,
    pub rejected: Vec<std::path::PathBuf>,
    pub conflicts: Vec<(std::path::PathBuf, String)>,
}

/// The retained outcome of a shadow settle that has not retired the shadow
/// (verification pending, integration blocked, completion refused): the live
/// row keeps re-pointing the session and a later settlement resumes.
fn retained_shadow_finalize() -> ShadowFinalize {
    ShadowFinalize {
        action: ShadowFinalizeAction::Retained,
        merged: Vec::new(),
        rejected: Vec::new(),
        conflicts: Vec::new(),
    }
}

impl TaskExecutor {
    /// The ONE production construction path: the daemon's shadow service is
    /// REQUIRED (mutating runs always execute in an isolated candidate), so
    /// no production value can disable isolation. The candidate-root
    /// allocator is rooted under the store's data directory (the daemon's
    /// own root — never a client path).
    pub fn new(
        orchestrator: &Arc<OrchestratorRuntime>,
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        shadows: Arc<ShadowRoots>,
    ) -> Arc<Self> {
        Self::assemble(orchestrator, session, agent, Some(shadows))
    }

    /// Test-harness seam: an executor with NO shadow service, for the
    /// LOW-LEVEL tests that genuinely drive the owner checkout directly
    /// (they predate shadow mutation and verify drive/durability semantics,
    /// not the isolation policy). It is gated by `cfg(test)`/debug
    /// assertions and is compiled OUT of release builds — production code
    /// cannot name it, and no runtime value can flip a production executor
    /// back to owner-direct. Never use it outside tests.
    #[cfg(any(test, debug_assertions))]
    pub fn new_owner_direct_for_test_harness(
        orchestrator: &Arc<OrchestratorRuntime>,
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
    ) -> Arc<Self> {
        Self::assemble(orchestrator, session, agent, None)
    }

    /// The shared assembly body. Private: `None` shadows is reachable ONLY
    /// through the cfg-gated test seam above.
    fn assemble(
        orchestrator: &Arc<OrchestratorRuntime>,
        session: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        shadows: Option<Arc<ShadowRoots>>,
    ) -> Arc<Self> {
        let run_roots = Self::default_run_roots(&session);
        Arc::new(Self {
            orchestrator: orchestrator.clone(),
            session,
            agent,
            active: Mutex::new(HashMap::new()),
            shadows,
            run_roots,
            completion_steps: Mutex::new(CompletionStepsWiring::default()),
            settlement_seam: Mutex::new(None),
            placement: Mutex::new(WorkerPlacement::disabled()),
        })
    }

    /// Install (or clear) the ONE-SHOT settlement crash seam (tests). The
    /// seam fires at the FIRST matching boundary of the next settlement and
    /// is consumed; clearing it lets the recovery pass run.
    pub fn set_settlement_crash_seam(&self, seam: Option<CrashSeam>) {
        *self.lock_settlement_seam() = seam.map(|s| (s, false));
    }

    /// Install the additive worker-plane placement seam (the daemon does
    /// this when the `[workers]` section is enabled). Never called in the
    /// default configuration, where placement stays disabled and local
    /// execution is unchanged.
    pub fn set_worker_placement(&self, placement: WorkerPlacement) {
        *self.lock_placement() = placement;
    }

    /// Whether the worker-plane placement seam is enabled.
    pub fn worker_placement_enabled(&self) -> bool {
        self.lock_placement().is_enabled()
    }

    /// The placement knob with classified recovery: the seam is a
    /// configuration value (the durable worker rows it drives remain the
    /// authority), so a poisoned guard is recovered by clearing the poison
    /// rather than wedging every task start.
    fn lock_placement(&self) -> std::sync::MutexGuard<'_, WorkerPlacement> {
        self.placement.lock().unwrap_or_else(|poisoned| {
            self.placement.clear_poison();
            poisoned.into_inner()
        })
    }

    /// One placement consultation. A disabled seam answers `Local`; an
    /// enabled seam's failure is typed ([`ExecError::PlacementRefused`]) and
    /// nothing local has been written yet.
    fn place_new_run(&self, spec: &PlacementSpec) -> Result<PlacementDecision, ExecError> {
        self.lock_placement()
            .place(spec)
            .map_err(ExecError::PlacementRefused)
    }

    /// The provider-neutral placement request of one new run: the job key is
    /// deterministic for the same session + goal (a replayed start maps onto
    /// the SAME immutable generation), the payload digest binds the exact
    /// goal text, and requirements start empty (the daemon's `[workers]`
    /// adapter overlays its configured requirement defaults).
    fn placement_spec_for(parent: SessionId, req: &TaskRunRequest) -> PlacementSpec {
        let digest = blake3::hash(req.goal.as_bytes()).to_hex().to_string();
        PlacementSpec {
            job_key: format!("session-{}-goal-{}", parent.raw(), &digest[..16]),
            organization: String::new(),
            trust_domain: String::new(),
            payload_digest: digest,
            kind: if req.work_items.len() == 1 {
                "in_session".to_string()
            } else {
                "orchestrated".to_string()
            },
            os: None,
            arch: None,
            toolchains: Vec::new(),
            sandbox: Vec::new(),
            network: None,
            min_cpu_cores: 0,
            min_memory_mb: 0,
            gpu: false,
            region: None,
        }
    }

    /// The settlement crash seam with classified recovery: the seam is a
    /// test-only DERIVED flag (the durable rows it simulates a crash across
    /// remain the authority), so a poisoned guard is recovered with the
    /// poison flag cleared rather than wedging the recovery pass.
    fn lock_settlement_seam(&self) -> std::sync::MutexGuard<'_, Option<(CrashSeam, bool)>> {
        self.settlement_seam.lock().unwrap_or_else(|poisoned| {
            self.settlement_seam.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Consume the configured settlement seam when `seam` matches it. The
    /// first match fires exactly once; later calls (the recovery pass) pass.
    fn check_settlement_seam(&self, seam: CrashSeam) -> Result<(), ExecError> {
        let mut guard = self.lock_settlement_seam();
        if let Some((configured, fired)) = guard.as_mut() {
            if *configured == seam && !*fired {
                *fired = true;
                return Err(ExecError::InjectedCrashSeam(format!("{seam:?}")));
            }
        }
        Ok(())
    }

    /// Create (once) the IMMUTABLE run base of one orchestrated run (point
    /// 2): a stable copy of the owner root accepted only when
    /// `before == after == copied`, with bounded retries and a typed
    /// [`ExecError::WorkspaceDrift`] when the owner keeps moving. The
    /// durable [`faktor_session::ledger::RunBaseRecord`] is written BEFORE
    /// the first child spawn; a replay converges on the recorded base and
    /// refuses a run whose owner/base moved away from it.
    fn create_run_base(
        &self,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        owner_root: &std::path::Path,
        base_root: &std::path::Path,
        workspace_id: u64,
        worktree_id: u64,
    ) -> Result<faktor_session::ledger::RunBaseRecord, ExecError> {
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        if let Some(existing) = durable_read_value(
            format!("run base of run {run_id}"),
            handle.ledger_run_base_read(run_id),
        )
        .map_err(ExecError::from)?
        {
            let copied = digest(base_root)?;
            let owner = digest(owner_root)?;
            if copied == existing.snapshot_hash && owner == existing.snapshot_hash {
                return Ok(existing);
            }
            return Err(ExecError::WorkspaceDrift(format!(
                "run {run_id} already recorded base {} but the owner digests to {owner} and the base copy to {copied}; a new generation is never silently minted",
                existing.snapshot_hash
            )));
        }
        let mut detail = String::new();
        for attempt in 1..=RUN_BASE_COPY_ATTEMPTS {
            if base_root.exists() {
                std::fs::remove_dir_all(base_root).map_err(|e| {
                    ExecError::Internal(format!("run base reset {}: {e}", base_root.display()))
                })?;
            }
            std::fs::create_dir_all(base_root).map_err(|e| {
                ExecError::Internal(format!("run base dir {}: {e}", base_root.display()))
            })?;
            let before = digest(owner_root)?;
            // The manifest-faithful copy (literal symlinks, executable-bit
            // preservation): the run base is the SAME canonical tree as the
            // owner, so a mode-bearing or symlinked owner can never be
            // silently re-shaped by the copy.
            let manifest = faktor_fs::tree_manifest::copy_tree_manifest(
                owner_root,
                base_root,
                MAX_RUN_BASE_ENTRIES,
                MAX_RUN_BASE_BYTES,
                RUN_BASE_SKIP_DIRS,
            )
            .map_err(|e| {
                ExecError::WorkspaceDrift(format!("run base copy of {}: {e}", owner_root.display()))
            })?;
            let after = digest(owner_root)?;
            let copied = digest(base_root)?;
            if before == after && after == copied {
                let rows: Vec<String> = manifest
                    .entries()
                    .iter()
                    .map(|e| format!("{}|{}", e.normalized_path, e.payload_digest))
                    .collect();
                let record = faktor_session::ledger::RunBaseRecord {
                    run_id: run_id.to_string(),
                    workspace_id,
                    worktree_id,
                    snapshot_hash: copied,
                    manifest_digest: stable_list_digest(&rows),
                    root: base_root.to_string_lossy().into_owned(),
                    created_ms: handle.now_ms(),
                };
                handle
                    .ledger_run_base_set(&record)
                    .map_err(|e| ExecError::Internal(format!("run base record write: {e}")))?;
                return Ok(record);
            }
            detail = format!("attempt {attempt}: before={before} after={after} copied={copied}");
        }
        let _ = std::fs::remove_dir_all(base_root);
        Err(ExecError::WorkspaceDrift(format!(
            "owner root {} did not stabilize during the run-base copy after {RUN_BASE_COPY_ATTEMPTS} attempts ({detail}); refusing to derive children from a drifting generation",
            owner_root.display()
        )))
    }

    /// Configure the PR/push/commit execution policy of the completion-step
    /// runner (the daemon's strict `[completion]` config section). The
    /// config is validated BEFORE it is stored; a cached runner is dropped so
    /// the next contracted run rebuilds with the new values. The policy lock
    /// is AUTHORITY state: a poisoned guard refuses the change typed
    /// ([`PoisonedAuthority`]), never half-applying a commit/push/PR policy.
    pub fn configure_completion_steps(
        &self,
        config: CompletionStepsConfig,
    ) -> Result<(), ExecError> {
        config.validate().map_err(ExecError::InvalidPlan)?;
        let mut wiring = self
            .completion_steps
            .lock()
            .map_err(PoisonedAuthority::completion_step_lock)?;
        wiring.config = config;
        wiring.runner = None;
        Ok(())
    }

    /// The completion-step wiring guard with classified recovery: the config
    /// is the daemon's validated POLICY (mutations refuse typed on poison,
    /// see [`Self::configure_completion_steps`]) while the cached runner is a
    /// DERIVED cache. A poisoned guard is recovered with the poison flag
    /// cleared and the cached runner DROPPED, so a torn config/runner pair
    /// can never execute the OLD policy; the next contracted run rebuilds
    /// the runner from the recovered, validated config.
    fn lock_completion_steps(&self) -> std::sync::MutexGuard<'_, CompletionStepsWiring> {
        self.completion_steps.lock().unwrap_or_else(|poisoned| {
            self.completion_steps.clear_poison();
            let mut guard = poisoned.into_inner();
            guard.runner = None;
            guard
        })
    }

    /// The configured completion-step policy.
    pub fn completion_steps_config(&self) -> CompletionStepsConfig {
        self.lock_completion_steps().config.clone()
    }

    /// Install an explicit runner (daemon wiring/tests). `None` clears it:
    /// the executor then lazily rebuilds from the agent's supervisor.
    pub fn set_completion_steps(&self, runner: Option<Arc<CompletionStepRunner>>) {
        self.lock_completion_steps().runner = runner;
    }

    /// The runner of this executor: an explicitly installed one, else one
    /// built from the agent's own process supervisor + sandbox egress gate.
    /// `None` when the daemon has no supervisor (nothing can be executed).
    fn completion_step_runner(&self) -> Result<Option<Arc<CompletionStepRunner>>, ExecError> {
        let mut wiring = self.lock_completion_steps();
        if let Some(runner) = &wiring.runner {
            return Ok(Some(runner.clone()));
        }
        let Some(supervisor) = self.agent.deps().supervisor.clone() else {
            return Ok(None);
        };
        let sandbox = self.agent.deps().sandbox.clone();
        let egress: Arc<dyn EgressPolicy> = Arc::new(move |url: &str| match &sandbox {
            Some(engine) => engine.check_egress(url).map_err(|e| e.to_string()),
            None => Ok(()),
        });
        let runner = Arc::new(
            CompletionStepRunner::new(supervisor, egress, wiring.config.clone()).map_err(|e| {
                ExecError::InvalidPlan(format!("completion-step runner config: {e}"))
            })?,
        );
        wiring.runner = Some(runner.clone());
        Ok(Some(runner))
    }

    /// The default candidate-root authority: `<store data dir>/candidate-runs`
    /// (`store.path()` is `<data dir>/store/faktor-plus.db`), falling back
    /// to the process temp dir only when the store path has no parent.
    fn default_run_roots(session: &SessionManager) -> Arc<CandidateWorkspaceService> {
        let base = session
            .store()
            .path()
            .parent()
            .map(|dir| dir.join("candidate-runs"))
            .unwrap_or_else(|| std::env::temp_dir().join("faktor-candidate-runs"));
        CandidateWorkspaceService::new(base)
    }

    /// The ONE candidate-root allocator of this executor: the daemon
    /// allocates every orchestrated run's isolated root here.
    pub fn run_roots(&self) -> &Arc<CandidateWorkspaceService> {
        &self.run_roots
    }

    /// The daemon's shadow service. Production always carries it (the
    /// production constructor requires it); `None` is the cfg-gated test
    /// seam only.
    pub fn shadows(&self) -> Option<Arc<ShadowRoots>> {
        self.shadows.clone()
    }

    pub fn orchestrator(&self) -> &Arc<OrchestratorRuntime> {
        &self.orchestrator
    }

    pub fn session(&self) -> &Arc<SessionManager> {
        &self.session
    }

    pub fn agent(&self) -> &Arc<AgentRuntime> {
        &self.agent
    }

    /// The active orchestrated run(s) being driven, newest-first
    /// (tests/UI). Runs of different parent sessions may coexist.
    pub fn active_runs(&self) -> Vec<(SessionId, String)> {
        match self.active_runs_checked() {
            Ok(runs) => runs,
            // The legacy view carries no error channel; a poisoned authority
            // is fatal here (typed consumers call
            // [`Self::active_runs_checked`] and refuse).
            Err(e) => panic!("{e}"),
        }
    }

    /// [`Self::active_runs`] with the typed [`PoisonedAuthority`] refusal:
    /// a poisoned active-run lock can never masquerade as "no active runs".
    pub fn active_runs_checked(&self) -> Result<Vec<(SessionId, String)>, PoisonedAuthority> {
        let mut runs: Vec<(SessionId, String)> = self
            .active_guard()?
            .values()
            .map(|a| (a.parent, a.run_id.clone()))
            .collect();
        runs.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(runs)
    }

    /// The active-run lock guard with the typed poison refusal.
    fn active_guard(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, ActiveRun>>, PoisonedAuthority> {
        self.active
            .lock()
            .map_err(PoisonedAuthority::active_run_lock)
    }

    /// Adversarial test seam: poison the active-run lock exactly as a
    /// panicking writer would (a panic while the guard is held), so the
    /// typed [`PoisonedAuthority`] refusal can be asserted end-to-end.
    #[cfg(test)]
    pub(crate) fn poison_active_run_lock_for_test(&self) {
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = self.active.lock().expect("fresh active-run lock");
            panic!("poison the active-run lock (test seam)");
        }));
        assert!(poisoned.is_err(), "the test seam must panic");
        assert!(
            self.active.lock().is_err(),
            "the active-run lock must be poisoned after the seam"
        );
    }

    /// The single active orchestrated run, if exactly one is being driven
    /// (legacy tests/UI view: with concurrent runs use [`Self::active_runs`]).
    pub fn active_run(&self) -> Option<(SessionId, String)> {
        self.active_runs().into_iter().next()
    }

    /// Start ONE task on the parent session. Dispatch (documented):
    ///
    /// - exactly one work item → [`Self::start_in_session`]: the plan's
    ///   work item owns the session's CURRENT workspace; the run drives the
    ///   session with the daemon's own drive entry (same submit, same
    ///   receipts/events as the direct prompt path), wrapped with a durable
    ///   task row + linkage row;
    /// - two or more work items → [`Self::start_orchestrated`]: a durable
    ///   plan row + REAL child sessions through `execute_task` (the
    ///   children run concurrently under the configured ceilings).
    ///
    /// Typed rejections: unknown/orchestrated-child sessions, sessions with
    /// a durable live run (crash residue — resume it first), a second
    /// concurrent orchestrated run, invalid plans/oversized input.
    pub fn start_task(
        self: &Arc<Self>,
        parent: SessionId,
        req: TaskRunRequest,
    ) -> Result<TaskRunReceipt, ExecError> {
        req.validate()?;
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        // Admit-time binary-attachment resolution (defense in depth behind
        // the server DTO): every digest must resolve to a byte-identical
        // durable row of THIS session BEFORE any shadow/run/task write —
        // an unknown or mismatched id can never leave a partial durable
        // admission behind. The EFFECTIVE (tri-state) set is admitted: a
        // `Some(vec![])` clear resolves nothing, and a replacing patch is
        // never shadowed by the stale legacy vector.
        handle
            .resolve_attachments(&req.effective_attachments_patch().unwrap_or_default())
            .map_err(|e| match e.kind {
                faktor_core::ErrorKind::NotFound => ExecError::NotFound(e.message),
                faktor_core::ErrorKind::Oversized => ExecError::Oversized(e.message),
                _ => ExecError::Malformed(e.message),
            })?;
        if handle.orchestrator_child_identity_get()?.is_some() {
            return Err(ExecError::InvalidState(
                "the session is itself an orchestrated child; tasks start on root sessions".into(),
            ));
        }
        // A deleted/ended session is a durable tombstone: it refuses new
        // turns (409), never a phantom run — checked BEFORE any identity
        // adoption or shadow work.
        if handle.row()?.lifecycle.is_terminal() {
            return Err(ExecError::Conflict(format!(
                "session {parent} is closed; new turns are refused"
            )));
        }
        // Work-entry unification: every session created by a protocol
        // surface starts with NO worktree row, and the shadow/multi-agent
        // paths need a registered owner root. The daemon adopts the
        // workspace's root as the session's owner worktree exactly once,
        // here, before any run decision — so Native, SDK compat and
        // ACP sessions all get the same owner identity without any adapter
        // constructing one.
        self.ensure_owner_identity(&handle)?;
        // Crash residue: a durable run with LIVE children must be resumed
        // (or cancelled) before this session accepts anything new — a fresh
        // run would otherwise orphan the mirror of the crashed one.
        let blockers = self.live_runs_of(parent)?;
        if !blockers.is_empty() {
            return Err(ExecError::Conflict(format!(
                "session {parent} has live orchestrated run(s) {} left by an interrupted executor; resume (TaskExecutor::resume_run) or cancel them before starting a new task",
                blockers.join(", ")
            )));
        }
        // Worker-plane placement (additive; DISABLED by default): consult
        // the seam BEFORE any local durable write of this run. A remote
        // decision mints/leases one immutable job generation in the worker
        // plane and starts NOTHING locally; a local decision (or a disabled
        // seam) falls through to the unchanged path below. A seam failure
        // is a typed refusal — never a silent local run.
        if self.worker_placement_enabled() {
            let spec = Self::placement_spec_for(parent, &req);
            match self.place_new_run(&spec)? {
                PlacementDecision::Remote {
                    job_id,
                    worker_id,
                    generation,
                    lease_id,
                } => {
                    tracing::info!(
                        session = parent.raw(),
                        job_id = %job_id,
                        worker_id = %worker_id,
                        generation,
                        lease_id = %lease_id,
                        "task run placed on a remote worker; no local execution started"
                    );
                    return Ok(TaskRunReceipt {
                        run_id: job_id,
                        mode: TaskRunMode::Remote,
                        op_id: None,
                        queued: true,
                    });
                }
                PlacementDecision::Local => {}
            }
        }
        // A LIVE durable shadow re-points every session file consumer at the
        // shadow root (`resolve_workspace_root`) — settle it deterministically
        // BEFORE ANY run of a session that carries one (a shadowed
        // single-item run and a multi-item run must never drive or
        // orchestrate over a stale live shadow). A shadow whose owner
        // integration is still pending refuses the new run typed.
        if self.shadows.is_some() {
            self.settle_existing_shadow(parent, &handle)?;
        }
        if req.work_items.len() == 1 {
            self.start_in_session(parent, req)
        } else {
            self.start_orchestrated(parent, req)
        }
    }

    /// Resume a run whose executor crashed or was interrupted: re-attach to
    /// the DURABLE rows (never memory), re-drive every non-terminal child
    /// from its recorded op, apply pending control rows exactly once, and
    /// drive to the run's outcome. Refuses when nothing is left to drive.
    pub fn resume_run(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: &str,
        ceilings: super::Ceilings,
        parent_caps: CapabilitySet,
        crash_seam: Option<CrashSeam>,
    ) -> Result<TaskRunReceipt, ExecError> {
        ceilings.validate().map_err(ExecError::InvalidPlan)?;
        // The durable plan row must exist and name the run.
        let _row = self
            .orchestrator
            .plan_row(parent, run_id)
            .map_err(|_| ExecError::NotFound(format!("run '{run_id}' under session {parent}")))?;
        let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, run_id)?;
        // (wave A3) A run whose assignment rows committed before its first
        // spawn is resumable: re-attach re-spawns every item under the id
        // its DURABLE assignment names. A run with neither children nor
        // assignments is nothing a re-attach can name.
        let has_assignments =
            !OrchestratorRuntime::assignment_rows(self.session.clone(), parent, run_id)?.is_empty();
        if rows.is_empty() && !has_assignments {
            return Err(ExecError::NotFound(format!(
                "run '{run_id}' has no durable children or work-item assignments"
            )));
        }
        if !rows.is_empty()
            && !rows
                .iter()
                .any(|c| !matches!(c.state, ChildState::Done | ChildState::Cancelled))
        {
            return Err(ExecError::Conflict(format!(
                "run '{run_id}' has no non-terminal children; nothing to resume"
            )));
        }
        self.occupy(parent, run_id)?;
        let orch = self.orchestrator.clone();
        let exec = self.clone();
        let run_id_owned = run_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = orch
                .reattach(
                    parent,
                    &run_id_owned,
                    ceilings,
                    parent_caps,
                    String::new(),
                    PathBuf::new(),
                    crash_seam,
                )
                .await
            {
                eprintln!("resumed-run re-attach failed for run {run_id_owned}: {e}");
            }
            // A re-attached run settles through the SAME common pass as a
            // fresh orchestrated run (idempotent: an already-settled run is
            // a no-op with the same outcome).
            if let Err(e) = exec
                .settle_run(RunSettlement::Orchestrated {
                    parent,
                    run_id: run_id_owned.clone(),
                })
                .await
            {
                eprintln!("resumed-run settlement failed for run {run_id_owned}: {e}");
            }
            // Drive finished: free the single-execution slot when it still
            // names this run.
            exec.clear_active_if(parent, &run_id_owned);
        });
        Ok(TaskRunReceipt {
            run_id: run_id.to_string(),
            mode: TaskRunMode::Orchestrated,
            op_id: None,
            queued: false,
        })
    }

    // ------------------------------------------------------------ internals

    fn occupy(&self, parent: SessionId, run_id: &str) -> Result<(), ExecError> {
        let mut guard = self.active_guard().map_err(ExecError::from)?;
        if guard.contains_key(run_id) {
            return Err(ExecError::Conflict(format!(
                "run '{run_id}' is already being driven"
            )));
        }
        // Parent-session index: ONE run per parent session. A second run of
        // the same parent is refused while another of its runs is active
        // (per-session sequential); runs of DIFFERENT sessions are never
        // serialized here — concurrency limits live in the scheduler
        // ceilings, the provider limits and the live-child ceiling.
        if let Some(other) = guard.values().find(|a| a.parent == parent) {
            return Err(ExecError::Conflict(format!(
                "another orchestrated run of session {parent} is active ('{}'); one run per parent session — resume or wait for it to finish",
                other.run_id
            )));
        }
        guard.insert(
            run_id.to_string(),
            ActiveRun {
                parent,
                run_id: run_id.to_string(),
            },
        );
        Ok(())
    }

    /// Free the run's slot after its drive ended (idempotent: only clears
    /// when the entry still names THIS run). A POISONED active-run lock
    /// cannot be consulted: the slot stays (the authority refuses, never
    /// silently "freed").
    fn clear_active_if(&self, parent: SessionId, run_id: &str) {
        let mut guard = match self.active_guard() {
            Ok(guard) => guard,
            Err(e) => {
                eprintln!("run slot of {run_id} not freed: {e}");
                return;
            }
        };
        if guard
            .get(run_id)
            .is_some_and(|a| a.parent == parent && a.run_id == run_id)
        {
            guard.remove(run_id);
        }
    }

    /// Every run under `parent` whose durable rows still carry LIVE
    /// children (Running/Waiting/Paused = crash residue or in flight) —
    /// plus runs whose work-item assignment rows committed but whose
    /// executor crashed BEFORE its first child spawned (their identity is
    /// durable; a new task must not orphan it — resume them instead).
    fn live_runs_of(&self, parent: SessionId) -> Result<Vec<String>, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut runs: BTreeSet<String> = BTreeSet::new();
        for (kind, key, _value) in parent_facts(&handle)? {
            match kind.as_str() {
                PLAN_ROW_KIND => {
                    runs.insert(key);
                }
                REGISTRY_ROW_KIND | ASSIGNMENT_ROW_KIND => {
                    if let Some(run) = key.rsplit_once('/').map(|(r, _c)| r) {
                        if !run.is_empty() {
                            runs.insert(run.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        let mut live = Vec::new();
        for run in runs {
            let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, &run)?;
            if rows.iter().any(|c| {
                matches!(
                    c.state,
                    ChildState::Running
                        | ChildState::Waiting
                        | ChildState::Paused
                        | ChildState::Blocked
                )
            }) {
                live.push(run);
                continue;
            }
            if rows.is_empty()
                && !OrchestratorRuntime::assignment_rows(self.session.clone(), parent, &run)?
                    .is_empty()
            {
                live.push(run);
            }
        }
        Ok(live)
    }

    /// The single-agent case: drive the EXISTING session with the daemon's
    /// own drive path. Byte compatibility: `agent.submit` first (the
    /// receipt carries the true queued state + real op id), then the same
    /// detached drive the prompt endpoints use (`run_session_queue` for
    /// queued receipts, `drive_receipt` otherwise).
    /// Ensure the session has a registered OWNER worktree: every protocol
    /// surface creates sessions without one, and the shadow + multi-agent
    /// paths resolve the owner root from durable worktree rows. The
    /// workspace's root is adopted exactly once (idempotent no-op when a
    /// worktree row exists); a workspace without a filesystem root is
    /// refused loudly.
    fn ensure_owner_identity(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<(), ExecError> {
        let row = handle.row()?;
        if !self.session.worktrees_of(row.workspace_id)?.is_empty() {
            return Ok(());
        }
        let root = self
            .session
            .workspace_root(row.workspace_id)?
            .ok_or_else(|| {
                ExecError::Conflict(format!(
                    "session {} workspace {} has no filesystem root; cannot establish the owner worktree",
                    handle.id(),
                    row.workspace_id.raw()
                ))
            })?;
        let path = root.to_string_lossy().into_owned();
        let wt = self
            .session
            .put_worktree(row.workspace_id, &path, "main")
            .map_err(|e| ExecError::Internal(format!("owner worktree row: {e}")))?;
        let task_id = if row.task_id.raw() == 0 {
            TaskId::new(1)
        } else {
            row.task_id
        };
        self.session
            .adopt_identity(handle.id(), WorktreeId::new(wt as u64), task_id)
            .map_err(|e| ExecError::Internal(format!("owner identity adoption: {e}")))?;
        Ok(())
    }

    fn start_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        req: TaskRunRequest,
    ) -> Result<TaskRunReceipt, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let item = &req.work_items[0];
        // P0 isolation: a MUTATING single-item run ALWAYS works in a
        // daemon-owned isolated candidate (the shadow machinery); the drive
        // itself is byte-identical (submit + the detached daemon drive), the
        // shadow only re-points where the session resolves files and gates
        // the integration commit. `req.mutation_mode` is decoded for wire
        // compatibility only — the sole decodable value is Shadow, and the
        // decision below never consults it. Only the cfg-gated test seam
        // (no shadow service) drives the owner directly.
        let shadowed = self.shadows.is_some() && item.kind.is_mutating();
        let base_root = if shadowed {
            Some(self.owner_root_of(parent, &handle)?)
        } else {
            None
        };
        // P0-48: begin the shadow BEFORE anything else is written — a
        // failed copy (typed Oversized, symlink escape, ...) refuses the
        // run before any turn exists, before any durable row of this run,
        // and before any byte of the user checkout could be touched.
        if let Some(base) = &base_root {
            let shadows = self.shadows.as_ref().expect("shadowed implies service");
            if let Err(e) = shadows.begin_shadow(parent, base) {
                let refusal = ExecError::from_shadow("shadow begin for session", e);
                if matches!(refusal, ExecError::Oversized(_)) {
                    // The BOUNDED caps refused an un-isolatable checkout
                    // (shadow settle phase 1). The turn can never run
                    // isolated, so it is admitted honestly and landed as a
                    // recoverable failure — never a hang, and never a
                    // promptable-but-idle machine. Protocol adapters project
                    // the typed state (the frozen wire answers its 502).
                    return self.admit_refused_isolation(parent, &handle, &req, refusal);
                }
                return Err(refusal);
            }
        }
        // Durable task row (wave 9/16): one row per session task. A fresh
        // session seeds with the run's goal; a non-terminal existing row is
        // re-goaled; a TERMINAL row is frozen (the task certified its
        // lifetime) — a new task needs a fresh session.
        let task_id = handle.task_id()?;
        let now = handle.now_ms();
        let goal = truncate_bytes(&req.goal, MAX_TASK_GOAL_BYTES);
        let existing = handle.get_task(task_id)?;
        match existing {
            Some(t) if t.state.is_terminal() => {
                return Err(ExecError::Conflict(format!(
                    "session task {task_id} is terminal ({:?}); its row is frozen once certified — start the task on a fresh session",
                    t.state
                )));
            }
            Some(_) => {
                // FIX 3 tri-state re-goal: `None` = continuation (the row
                // keeps its durable criteria/attachment set), `Some(vec![])`
                // = explicitly CLEAR, `Some(items)` = REPLACE. The dedicated
                // patch fields win; the legacy vectors keep their historical
                // empty-means-continuation mapping through the effective
                // accessors. Re-goaling keeps the SAME durable TaskId: every
                // settlement path re-reads the session's task identity via
                // `handle.task_id()`, so minting a new id for a new goal mid-
                // conversation would strand an in-flight run's settlement on
                // the wrong task row. A distinctly new goal instead lands a
                // new CONTRACT (fresh criteria/attachments + a revision bump
                // that invalidates prior proofs) on that identity.
                handle
                    .update_task(
                        task_id,
                        faktor_session::TaskPatch {
                            goal: Some(goal.clone()),
                            acceptance_criteria: req.effective_criteria_patch(),
                            attachments: req.effective_attachments_patch(),
                            ..Default::default()
                        },
                    )
                    .map_err(|e| ExecError::Internal(format!("task row goal update: {e}")))?;
            }
            None => {
                handle
                    .create_task(faktor_session::Task {
                        task_id,
                        session_id: parent,
                        goal,
                        // A fresh row has nothing to continue: `None` seeds
                        // empty (the effective accessor's continuation
                        // policy); an explicit patch seeds its items.
                        acceptance_criteria: req.effective_criteria_patch().unwrap_or_default(),
                        plan: Vec::new(),
                        attachments: req.effective_attachments_patch().unwrap_or_default(),
                        budget: TaskBudget {
                            max_tokens: req.max_tokens,
                            max_turns: None,
                            spent_tokens: 0,
                            spent_turns: 0,
                        },
                        state: TaskState::Pending,
                        created_ms: now,
                        updated_ms: now,
                    })
                    .map_err(|e| ExecError::Internal(format!("task row seed: {e}")))?;
            }
        }
        // P2 record-first: the accepted completion contract lands durably
        // BEFORE the run's first model call (the submit below drives it).
        record_completion_contract(&handle, task_id, req.completion_contract)?;
        // Durable monetary cap (audit 9/H): `TaskRunRequest.max_cost_micro`
        // flows to the task row's cost cap — the single authority every paid
        // model call of this drive is admitted against (the guarded ledger
        // set refuses a reduction below what the row already committed:
        // spend never rewinds). `None` leaves whatever cap the row carries;
        // only `Some` writes.
        if let Some(max_cost_micro) = req.max_cost_micro {
            faktor_session::DurableBudgetLedger::new(self.session.clone())
                .set_task_max_cost(parent, task_id, Some(max_cost_micro))
                .map_err(|e| ExecError::Conflict(format!("task cost cap seed: {e}")))?;
        }
        if let Some(mt) = req.max_tokens {
            self.agent
                .seed_task_budget(
                    parent,
                    &TaskBudget {
                        max_tokens: Some(mt),
                        max_turns: None,
                        spent_tokens: 0,
                        spent_turns: 0,
                    },
                )
                .map_err(|e| ExecError::Internal(format!("budget seed: {}", e.message)))?;
        }
        let receipt = self
            .agent
            .submit(parent, &req.goal, &req.files)
            .map_err(|e| ExecError::Internal(format!("submit: {}", e.message)))?;
        let run_id = format!("tx-{:016x}", receipt.op_id.raw());
        let row = TaskRunRow {
            run_id: run_id.clone(),
            session_id: parent.raw(),
            mode: TaskRunMode::InSession,
            goal: truncate(&req.goal, MAX_GOAL_CHARS),
            item_ids: vec![item.id.clone()],
            files: req.files.clone(),
            // The run row carries the EFFECTIVE (tri-state) attachment set:
            // `Some(vec![])` records a cleared set, never the stale legacy
            // vector it overrides.
            attachments: req.effective_attachments_patch().unwrap_or_default(),
            op_id: Some(receipt.op_id.raw()),
            model: req.model.clone(),
            budget_max_tokens: req.max_tokens,
            created_ms: now,
        };
        put_run_row(&handle, &run_id, &row)?;
        // P0 no-op policy: the run's disposition is durable BEFORE the drive
        // is dispatched. The shadow settlement reads it under the synthetic
        // per-session run id (`tx-session-<session>`), so the row is written
        // under BOTH the real run id and that id.
        write_run_policy_row(&handle, &run_id, req.no_op_disposition)?;
        write_run_policy_row(
            &handle,
            &format!("tx-session-{}", parent.raw()),
            req.no_op_disposition,
        )?;
        // Detached drive — the daemon's own entries, identical to the
        // direct prompt path (the drive runs session recovery first; an
        // interrupted drive resumes the SAME recorded turn on daemon start).
        // Shadowed runs additionally finalize the shadow once the drive
        // returns (integrate on verified-complete, discard on failure).
        let exec = self.clone();
        if receipt.queued {
            let agent = self.agent.clone();
            tokio::spawn(async move {
                agent.run_session_queue(parent).await;
                exec.after_shadowed_drive(parent).await;
            });
        } else {
            let agent = self.agent.clone();
            let model = req.model.clone();
            let handle2 = self.session.get_session(parent).ok().flatten();
            if let Some(h) = handle2 {
                let receipt2 = receipt.clone();
                tokio::spawn(async move {
                    if let Err(e) = agent.drive_receipt(&h, receipt2, model).await {
                        eprintln!("in-session drive failed for session {parent}: {e}");
                    }
                    exec.after_shadowed_drive(parent).await;
                });
            }
        }
        Ok(TaskRunReceipt {
            run_id,
            mode: TaskRunMode::InSession,
            op_id: Some(receipt.op_id),
            queued: receipt.queued,
        })
    }

    /// Admit a single-item run whose isolation candidate the BOUNDED copy
    /// caps refused ([`ExecError::Oversized`] from `begin_shadow`): the
    /// durable user prompt is recorded, the turn lands
    /// [`faktor_core::state::AgentState::FailedRecoverable`] (its record
    /// closed), and the receipt is returned so every protocol adapter
    /// projects the typed failure promptly. No task, run or shadow row is
    /// written — the run never existed.
    fn admit_refused_isolation(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
        req: &TaskRunRequest,
        refusal: ExecError,
    ) -> Result<TaskRunReceipt, ExecError> {
        let receipt = self
            .agent
            .submit(parent, &req.goal, &req.files)
            .map_err(|e| ExecError::Internal(format!("submit: {}", e.message)))?;
        if receipt.queued {
            // A queued prompt never ran under this attempt; never strand a
            // queue row whose isolation candidate does not exist. The row is
            // cancelled and the refusal stays typed.
            let _ = handle.abort(Some(receipt.op_id));
            return Err(refusal);
        }
        let message = format!("isolation candidate refused: {refusal}");
        if handle
            .append_event(
                faktor_core::event::EventKind::Failed,
                faktor_core::state::AgentState::FailedRecoverable,
                Some(receipt.op_id),
                Some(serde_json::json!({ "message": message })),
            )
            .is_err()
        {
            // The machine could not record the failure: settle it through the
            // abort transition (a turn outcome, always legal from a mid-turn
            // state) and keep the typed refusal.
            let _ = handle.abort(Some(receipt.op_id));
            return Err(refusal);
        }
        let _ = handle.finish_turn_record(receipt.op_id, "failed");
        // The durable run row makes the admitted-but-undriven run readable
        // through the exact projection every other run uses (state derives
        // from the settled session/task rows) — a client that holds the
        // receipt never faces a phantom 404.
        let run_id = format!("tx-{:016x}", receipt.op_id.raw());
        let row = TaskRunRow {
            run_id: run_id.clone(),
            session_id: parent.raw(),
            mode: TaskRunMode::InSession,
            goal: truncate(&req.goal, MAX_GOAL_CHARS),
            item_ids: req.work_items.iter().map(|w| w.id.clone()).collect(),
            files: req.files.clone(),
            attachments: req.effective_attachments_patch().unwrap_or_default(),
            op_id: Some(receipt.op_id.raw()),
            model: req.model.clone(),
            budget_max_tokens: req.max_tokens,
            created_ms: handle.now_ms(),
        };
        put_run_row(handle, &run_id, &row)?;
        tracing::info!(
            target: "faktor::task_executor",
            session = %parent,
            "shadow phase refused (bounded caps); turn admitted and landed failed_recoverable: {message}"
        );
        Ok(TaskRunReceipt {
            run_id,
            mode: TaskRunMode::InSession,
            op_id: Some(receipt.op_id),
            queued: false,
        })
    }

    /// The multi-agent case: a durable plan row + REAL child sessions
    /// through `execute_task`, driven in the background (the receipt is
    /// returned once the plan is durably registered; children appear under
    /// the run id immediately afterwards).
    fn start_orchestrated(
        self: &Arc<Self>,
        parent: SessionId,
        req: TaskRunRequest,
    ) -> Result<TaskRunReceipt, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let row = handle.row()?;
        let owner_root = {
            let wts = self
                .session
                .worktrees_of(row.workspace_id)?
                .into_iter()
                .filter(|w| (w.id as u64) == row.worktree_id.raw())
                .map(|w| PathBuf::from(w.path))
                .collect::<Vec<_>>();
            wts.first().cloned().ok_or_else(|| {
                ExecError::Conflict(format!(
                    "session {parent} has no registered worktree row; multi-item runs need a real owner worktree"
                ))
            })?
        };
        let provider = row.provider.clone();
        let default_model = req.model.clone().unwrap_or_else(|| row.model.clone());
        if provider.is_empty() || default_model.is_empty() {
            return Err(ExecError::Conflict(format!(
                "session {parent} carries no provider/model; cannot orchestrate"
            )));
        }
        // Point 7: EVERY orchestrated run owns a durable ROOT task row,
        // created (or re-goaled) BEFORE the first child spawn or model call.
        // There is no conditional concept and no settlement path for a run
        // without a root row to certify; a terminal row stays frozen.
        let task_id = handle.task_id()?;
        {
            let now = handle.now_ms();
            let goal = truncate_bytes(&req.goal, MAX_TASK_GOAL_BYTES);
            match handle.get_task(task_id)? {
                Some(t) if t.state.is_terminal() => {
                    return Err(ExecError::Conflict(format!(
                        "session task {task_id} is terminal ({:?}); its row is frozen once certified — start the task on a fresh session",
                        t.state
                    )));
                }
                Some(_) => {
                    // FIX 3 tri-state re-goal (same contract as the
                    // in-session arm): None = continuation, Some([]) =
                    // clear, Some(items) = replace.
                    handle
                        .update_task(
                            task_id,
                            faktor_session::TaskPatch {
                                goal: Some(goal.clone()),
                                acceptance_criteria: req.effective_criteria_patch(),
                                attachments: req.effective_attachments_patch(),
                                ..Default::default()
                            },
                        )
                        .map_err(|e| ExecError::Internal(format!("root task row update: {e}")))?;
                }
                None => {
                    handle
                        .create_task(faktor_session::Task {
                            task_id,
                            session_id: parent,
                            goal,
                            acceptance_criteria: req.effective_criteria_patch().unwrap_or_default(),
                            plan: Vec::new(),
                            attachments: req.effective_attachments_patch().unwrap_or_default(),
                            budget: faktor_session::TaskBudget::default(),
                            state: TaskState::Pending,
                            created_ms: now,
                            updated_ms: now,
                        })
                        .map_err(|e| ExecError::Internal(format!("root task row seed: {e}")))?;
                }
            }
        }
        if let Some(max_cost_micro) = req.max_cost_micro {
            faktor_session::DurableBudgetLedger::new(self.session.clone())
                .set_task_max_cost(parent, task_id, Some(max_cost_micro))
                .map_err(|e| ExecError::Conflict(format!("root task cost cap seed: {e}")))?;
        }
        // P2 record-first: the run's contract lands durably BEFORE any child
        // session (its first model call) is spawned.
        record_completion_contract(&handle, task_id, req.completion_contract)?;
        let plan = req.plan_for_validation();
        let mut specs = Vec::with_capacity(req.work_items.len());
        for w in &req.work_items {
            let mut s = ChildSpec::new(w.id.clone());
            s.spawn = !req.auto_items.iter().any(|a| a == &w.id);
            s.max_tokens = req.max_tokens;
            // The run's attachments are part of EVERY child spec: the plan
            // row persists them BEFORE any spawn and re-attach decodes the
            // byte-identical set (never memory). `validate()` already
            // enforced the shared bounds/hostile rules.
            s.files = req.files.clone();
            // The run's BINARY attachment set rides every child spec too —
            // SEPARATE from the workspace-relative `files`; the durable plan
            // row reconstructs the exact typed set on re-attach. The
            // EFFECTIVE (tri-state) set is used: `Some(vec![])` carries a
            // cleared set, never the stale legacy vector it overrides.
            s.attachments = req.effective_attachments_patch().unwrap_or_default();
            // (audits 7/8/21/22, work-entry unification) Ownership is read
            // from the ITEM alone and lands on the durable wave-A3
            // assignment rows at compile (before any spawn); the child spec
            // never carries ownership. File-level capability follows the
            // item's ownership: a semantic-entity item's writes are
            // provider-scoped — it gets READ-only file capability, never
            // WriteWorkspace on the shared worktree. Everything else keeps
            // the kind-derived caps.
            let semantic = matches!(w.ownership, OwnershipSpec::SemanticEntities { .. });
            s.task_caps = if semantic {
                read_child_caps()
            } else {
                child_caps(w.kind)
            };
            s.child_caps = s.task_caps.clone();
            specs.push(s);
        }
        let run_id = format!("run-{:016x}", self.session.next_op_id().raw());
        // P0 no-op policy: the run's disposition is durable BEFORE the run is
        // claimed or any child spawns, so the detached settlement applies the
        // exact policy the caller requested.
        write_run_policy_row(&handle, &run_id, req.no_op_disposition)?;
        self.occupy(parent, &run_id)?;
        // The DAEMON allocates the isolated root itself (never an HTTP
        // path): one CandidateWorkspaceService authority per executor. A
        // test/programmatic caller may pass an explicit root; the wire
        // request already leaves it empty.
        let isolated_root = if req.isolated_root.as_os_str().is_empty() {
            self.run_roots.allocate(parent, &run_id)?
        } else {
            req.isolated_root.clone()
        };
        // Point 2: the IMMUTABLE run base is created BEFORE the first child
        // spawn — a stable copy of the owner root at run start (before ==
        // after == copied, bounded retries, typed WorkspaceDrift). Every
        // isolated child and the integration candidate derive from THIS
        // generation; the live owner is never a staging source.
        let run_exec_dir = isolated_root.join(&run_id);
        std::fs::create_dir_all(&run_exec_dir)
            .map_err(|e| ExecError::Internal(format!("run exec dir {run_exec_dir:?}: {e}")))?;
        let base_root = run_exec_dir.join("base");
        if let Err(e) = self.create_run_base(
            &handle,
            &run_id,
            &owner_root,
            &base_root,
            row.workspace_id.raw(),
            row.worktree_id.raw(),
        ) {
            self.clear_active_if(parent, &run_id);
            return Err(e);
        }
        if let Err(e) = self.check_settlement_seam(CrashSeam::AfterRunBaseRecorded) {
            self.clear_active_if(parent, &run_id);
            return Err(e);
        }
        let orch = self.orchestrator.clone();
        let exec = self.clone();
        let owner = super::OwnerContext {
            parent_session: parent,
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root: owner_root,
        };
        let config = ExecConfig {
            run_id: run_id.clone(),
            ceilings: req.ceilings.clone(),
            parent_caps: req.parent_caps.clone(),
            provider,
            default_model,
            isolated_root,
            crash_seam: req.crash_seam,
        };
        let run_id2 = run_id.clone();
        tokio::spawn(async move {
            if let Err(e) = orch.execute_task(plan, owner, config, &specs).await {
                eprintln!("orchestrated run {run_id2} failed: {e}");
            }
            // THE post-run settlement (never an await-then-clear): the
            // aggregate root verification, the accepted contract's steps and
            // the completion gate all run before the active slot is freed.
            if let Err(e) = exec
                .settle_run(RunSettlement::Orchestrated {
                    parent,
                    run_id: run_id2.clone(),
                })
                .await
            {
                eprintln!("orchestrated-run settlement failed for run {run_id2}: {e}");
            }
            exec.clear_active_if(parent, &run_id2);
        });
        Ok(TaskRunReceipt {
            run_id,
            mode: TaskRunMode::Orchestrated,
            op_id: None,
            queued: false,
        })
    }

    // ------------------------------------------- completion steps (P2 follow-up)

    /// Execute the durable completion contract's requested steps (ordered
    /// commit, push, pr) for one session's current task, AUTHORIZED by the
    /// immutable verification record `proof` — the P0 proof-validated API.
    /// Every step revalidates the proof binding (task/revision/status/
    /// coverage/snapshot/current-root digest) immediately before it runs; a
    /// moved root records the step `Invalidated` (retryable refusal), never a
    /// blind side effect. There is NO fact-based authorization path: the
    /// durable `verification`/`last` fact is reporting state only.
    ///
    /// Additive invocation: the executor calls this from the post-drive
    /// settlement of a contracted run (deterministic verification has run);
    /// an uncontracted run returns `Ok(None)` BEFORE any runner exists or
    /// any git/supervisor work happens — the default path is byte-identical.
    pub async fn run_completion_steps(
        &self,
        parent: SessionId,
        proof: VerificationRecordId,
    ) -> Result<Option<CompletionStepReport>, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        // The run's integration/candidate root: the LIVE shadow root when
        // the session has one, else its durable owner worktree root.
        let root = match self.session.active_root(parent)? {
            Some(root) => root,
            None => self.owner_root_of(parent, &handle)?,
        };
        self.run_completion_steps_at(parent, proof, root).await
    }

    /// The steps of a VERIFIED orchestrated landing: execute the accepted
    /// contract against the LANDED owner root after the whole-root equality
    /// proof holds, authorized by the landing's verification record (the
    /// proof the landing itself consumed).
    pub async fn run_completion_steps_against_proof(
        &self,
        parent: SessionId,
        landed: &LandedRunIntegration,
    ) -> Result<Option<CompletionStepReport>, ExecError> {
        let _handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let current = root_manifest_digest(&landed.prepared.owner_root)?;
        if current != landed.landed_snapshot
            || landed.landed_snapshot != landed.prepared.candidate_snapshot
        {
            return Err(ExecError::WorkspaceDrift(format!(
                "completion steps refused: owner root {} no longer equals the verified candidate snapshot {}",
                landed.prepared.owner_root.display(),
                landed.prepared.candidate_snapshot
            )));
        }
        self.run_completion_steps_at(parent, landed.record, landed.prepared.owner_root.clone())
            .await
    }

    /// The shared completion-step body: contract lookup, terminal check and
    /// the PROOF-VALIDATED runner invocation against one explicit root.
    async fn run_completion_steps_at(
        &self,
        parent: SessionId,
        proof: VerificationRecordId,
        root: PathBuf,
    ) -> Result<Option<CompletionStepReport>, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle.task_id()?;
        let Some((_revision, contract)) = handle
            .completion_contract(task_id)
            .map_err(|e| ExecError::Internal(format!("completion contract read: {e}")))?
        else {
            return Ok(None);
        };
        if contract.is_default() {
            return Ok(None);
        }
        let Some(task) = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("completion task read: {e}")))?
        else {
            return Ok(None);
        };
        if task.state.is_terminal() {
            return Ok(None);
        }
        let Some(runner) = self.completion_step_runner()? else {
            return Err(ExecError::Conflict(
                "completion steps are requested but the daemon has no process supervisor; no step can be executed".into(),
            ));
        };
        let report = runner
            .run_completion_steps(
                &handle,
                task_id,
                proof,
                &CompletionStepContext {
                    root,
                    goal: task.goal,
                },
            )
            .await
            .map_err(|e| ExecError::Internal(format!("completion steps: {e}")))?;
        Ok(Some(report))
    }

    /// The newest durable PASSED verification record of the session's task at
    /// its CURRENT revision — the immutable basis an in-session completion
    /// step may be authorized by. `Ok(None)` (no passing record, or only
    /// records for an older revision) runs NOTHING: a completion contract
    /// never advances from an advisory fact. FIX 2: a store failure is an
    /// error, never "no passing proof".
    fn latest_passing_proof(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
    ) -> Result<Option<VerificationRecordId>, ExecError> {
        let revision = handle.task_revision(task_id).map_err(|e| {
            ExecError::from(classify_session_read(
                "task revision of the completion-step proof",
                e.into(),
            ))
        })?;
        let records = handle.list_verification_records(task_id).map_err(|e| {
            ExecError::from(classify_session_read(
                "verification records of the completion-step proof",
                e.into(),
            ))
        })?;
        Ok(records
            .into_iter()
            .filter(|record| {
                record.status == VerificationStatus::Passed && record.revision == revision
            })
            .max_by_key(|record| record.record_id)
            .map(|record| record.record_id))
    }

    // ------------------------------------------------- post-run settlement (P1)

    /// The ONE post-run settlement, shared by the single-session shadowed
    /// path (today's `after_shadowed_drive`: the drive owns verification +
    /// the completion gate, settlement runs the contract steps and the
    /// shadow finalize — byte-identical ordering preserved) and the
    /// orchestrated path (aggregate root verification, contract steps, the
    /// `complete_verified_task` gate, then finalize with explicit-merge-only
    /// proposals). Every decision reads durable rows, so a crashed executor
    /// re-runs the SAME pass on reopen and converges without double side
    /// effects (the step runner's replay semantics).
    pub async fn settle_run(
        self: &Arc<Self>,
        run: RunSettlement,
    ) -> Result<SettlementOutcome, ExecError> {
        match run {
            RunSettlement::InSession { parent, run_id } => {
                self.settle_in_session(parent, run_id).await
            }
            RunSettlement::Orchestrated { parent, run_id } => {
                self.settle_orchestrated(parent, run_id).await
            }
        }
    }

    /// Re-settle the runs of `parent` whose EXACT root verification attempt
    /// just became terminal (the daemon verification executor calls this
    /// after it resolves jobs). Without it a run parked on a pending attempt
    /// would never consume its result. Additive and convergent: an actively
    /// driven run is skipped (its drive settles itself), a run with open jobs
    /// or without a persisted root attempt is skipped, and `settle_run` on an
    /// already-settled run is a deterministic no-op.
    pub async fn settle_resolved_verifications(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<(), ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle
            .task_id()
            .map_err(|e| ExecError::Internal(format!("task id read: {e}")))?;
        // FIX 2: the verification-fact scan is an explicit durable read — a
        // store failure is an error (never "no runs to settle") and a
        // corrupt fact page refuses typed.
        let runs: Vec<String> = match handle.memory_facts() {
            Ok(facts) => facts
                .into_iter()
                .filter(|(kind, key, _)| kind == ROOT_ATTEMPT_FACT_KIND && key.starts_with("root:"))
                .filter_map(|(_, key, _)| key.strip_prefix("root:").map(str::to_string))
                .filter(|run| !run.is_empty())
                .collect(),
            Err(e) => {
                return Err(ExecError::from(classify_session_read(
                    "verification-attempt facts of the post-run settlement",
                    e,
                )))
            }
        };
        for run_id in runs {
            // A poisoned active-run lock must REFUSE the settlement pass:
            // treating it as "active" would silently skip work and treating
            // it as "empty" would double-drive a run.
            let active = match self.active_guard() {
                Ok(guard) => guard.contains_key(&run_id),
                Err(e) => return Err(ExecError::from(e)),
            };
            if active {
                continue;
            }
            // FIX 2: open verification jobs are read explicitly. A failed
            // read must NEVER collapse to "no open jobs" (which would let
            // the settlement race a still-open verification). A corrupt job
            // row refuses typed.
            let open = match handle.open_verification_jobs(task_id.raw()) {
                Ok(jobs) => jobs,
                Err(e) => {
                    return Err(ExecError::from(classify_session_read(
                        "open verification jobs of the post-run settlement",
                        e.into(),
                    )))
                }
            };
            if !open.is_empty() {
                continue;
            }
            if let Err(e) = self
                .settle_run(RunSettlement::Orchestrated {
                    parent,
                    run_id: run_id.clone(),
                })
                .await
            {
                eprintln!("post-executor settlement of run {run_id} failed: {e}");
            }
        }
        Ok(())
    }

    /// The in-session arm of [`Self::settle_run`] — the SAME ordering the
    /// orchestrated arm enforces:
    ///
    /// - a session carrying a LIVE managed shadow settles through
    ///   prepare(candidate from the immutable shadow run base) -> verify
    ///   (candidate) -> land(owner, transactional) -> completion steps
    ///   against the landed proof -> `complete_verified_task`. The drive's
    ///   shadow-world verification is never the permission to complete:
    ///   `VerifiedComplete` is the CONSEQUENCE of a successful owner landing;
    /// - any other in-session run (read-only, a retired shadow) keeps the
    ///   drive's own deterministic verification + gate byte-identically:
    ///   this arm runs the accepted contract's requested steps (fail-closed
    ///   on the durable verification fact) and the shadow lifecycle settle.
    ///
    /// A step failure is logged, never escalated: the durable rows record
    /// the typed outcome and a later settlement retries.
    async fn settle_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
    ) -> Result<SettlementOutcome, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle.task_id()?;
        if let Some(shadows) = self.shadows() {
            if let Some(row) = shadows.active_shadow(parent)? {
                if row.state.is_live() {
                    return self
                        .settle_shadowed_in_session(parent, run_id, &handle, task_id, row)
                        .await;
                }
            }
        }
        // Proof-validated steps: only a durable PASSED record at the task's
        // CURRENT revision authorizes any side effect. No passing proof (or
        // only stale-revision proof) runs nothing — the durable fact is
        // reporting state, never authorization.
        let steps = match self.latest_passing_proof(&handle, task_id)? {
            Some(proof) => match self.run_completion_steps(parent, proof).await {
                Ok(report) => report,
                Err(e) => {
                    eprintln!("completion-step execution failed for session {parent}: {e}");
                    None
                }
            },
            None => {
                // FIX 2: the contract read is explicit. A corrupt contract
                // refuses the settlement typed; a store failure is an error;
                // only a genuinely absent/default contract is "nothing to
                // report".
                let has_contract = match handle.completion_contract(task_id) {
                    Ok(Some((_, contract))) => !contract.is_default(),
                    Ok(None) => false,
                    Err(faktor_session::TaskError::CorruptDurableState { what, detail }) => {
                        return Err(ExecError::from(DurableStateError::CorruptDurableState {
                            what,
                            detail,
                        }))
                    }
                    Err(e) => {
                        return Err(ExecError::from(classify_session_read(
                            "completion contract of the in-session settlement",
                            e.into(),
                        )))
                    }
                };
                if has_contract {
                    eprintln!(
                        "completion steps for session {parent} are refused: no durable PASSED verification record at the current revision authorizes them; nothing is committed, pushed or opened"
                    );
                }
                None
            }
        };
        let finalize = self.finalize_shadow_run(parent)?;
        let task_state = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?
            .map(|t| t.state);
        let completed = task_state == Some(TaskState::VerifiedComplete);
        Ok(SettlementOutcome {
            run_id,
            orchestrated: false,
            complete: task_state.is_some_and(|s| s.is_terminal()),
            verified: verification_passed(&handle),
            completed,
            verification: None,
            steps,
            merge_proposals: Vec::new(),
            finalize,
        })
    }

    /// The single-item SHADOW arm of [`Self::settle_run`]: a one-child
    /// instance of the normal run-base/candidate/integration architecture.
    ///
    /// 1. **prepare** ([`Self::prepare_shadow_integration`]): the shadow's
    ///    candidate (its change set bound to the shadow's immutable run base)
    ///    is staged against the durable generation; the OWNER is untouched;
    /// 2. **verify** ([`Self::verify_prepared_integration`]) over the
    ///    CANDIDATE root (the shadow tree: base + the one child's changes),
    ///    creating the candidate-bound proof record;
    /// 3. **land** ([`Self::land_verified_integration`]): the transactional
    ///    owner landing (record-first per-path decisions + rollback blobs,
    ///    per-path CAS, whole-root equality). A conflict rolls every applied
    ///    path back — the owner is byte-identical to pre-landing;
    /// 4. **completion steps against the landed proof**, then the durable
    ///    completion gate (`complete_verified_task`);
    /// 5. **retire** the shadow row (record-first `Integrated` + directory
    ///    removal) ONLY after the owner holds the verified candidate.
    ///
    /// A landing conflict retains the shadow as `IntegrationBlocked` and
    /// leaves the task NON-TERMINAL; a later settlement retries (the bounded
    /// watcher, the next run's deterministic settle, or an explicit
    /// re-settle). The shadow-world certification the drive ran can never
    /// complete the task: while a managed shadow is live the session
    /// completion gate refuses an unbound proof.
    async fn settle_shadowed_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        row: ShadowRow,
    ) -> Result<SettlementOutcome, ExecError> {
        let shadows = self
            .shadows()
            .ok_or_else(|| ExecError::Internal("shadow settlement requires the service".into()))?;
        let mut outcome = SettlementOutcome {
            run_id: run_id.clone(),
            orchestrated: false,
            complete: false,
            verified: false,
            completed: false,
            verification: None,
            steps: None,
            merge_proposals: Vec::new(),
            finalize: None,
        };
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        let Some(task) = task else {
            return Ok(outcome);
        };
        match task.state {
            TaskState::Failed | TaskState::Cancelled => {
                shadows
                    .discard(parent)
                    .map_err(|e| ExecError::from_shadow("shadow discard", e))?;
                outcome.complete = true;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Discarded,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                });
                return Ok(outcome);
            }
            TaskState::VerifiedComplete => {
                // Crash window: the owner landing succeeded and the task was
                // completed, but the shadow row was not retired. The landing
                // transaction recovery is idempotent; nothing is re-verified
                // against a moved owner.
                let prepared =
                    self.prepare_shadow_integration(handle, parent, &run_id, task_id, &row)?;
                let proof = self
                    .recover_shadow_landing(handle, task_id, &prepared)
                    .await?;
                let Some(proof) = proof else {
                    outcome.finalize = Some(retained_shadow_finalize());
                    return Ok(outcome);
                };
                let landed = self.land_verified_integration(handle, &proof)?;
                outcome.verification = Some(proof.record());
                outcome.verified = true;
                shadows
                    .mark_integrated(parent)
                    .map_err(|e| ExecError::from_shadow("shadow retirement", e))?;
                outcome.completed = true;
                outcome.complete = true;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Integrated,
                    merged: landed.prepared.changed.iter().map(PathBuf::from).collect(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                });
                return Ok(outcome);
            }
            _ => {}
        }
        if !self.route_root_to_verifying(handle, task_id)? {
            outcome.finalize = Some(retained_shadow_finalize());
            return Ok(outcome);
        }
        // (1) PREPARE from the immutable shadow run base.
        let prepared = self.prepare_shadow_integration(handle, parent, &run_id, task_id, &row)?;
        self.check_settlement_seam(CrashSeam::AfterCandidatePrepared)?;
        // (2) VERIFY the CANDIDATE (the shadow tree), under the run's no-op
        // policy: an empty change set completes only through the reviewer's
        // no-op proof (or the explicit Allowed disposition). The criteria are
        // the task row already loaded above — never a second read whose
        // absence could silently degrade to "no criteria".
        let criteria = task.acceptance_criteria.clone();
        let no_op = run_no_op_disposition(handle, &run_id)?;
        let Some(proof) = self
            .verify_prepared_integration(handle, task_id, &criteria, &prepared, no_op)
            .await?
        else {
            outcome.finalize = Some(retained_shadow_finalize());
            return Ok(outcome);
        };
        outcome.verification = Some(proof.record());
        outcome.verified = true;
        // (3) LAND the verified candidate transactionally.
        let landed = match self.land_verified_integration(handle, &proof) {
            Ok(landed) => landed,
            Err(ExecError::IntegrationConflict(reason)) => {
                // The owner is byte-identical to pre-landing (every applied
                // path rolled back); the shadow is retained and the task
                // stays non-terminal until the drift is resolved.
                shadows
                    .mark_integration_blocked(parent)
                    .map_err(|e| ExecError::from_shadow("shadow block", e))?;
                outcome.finalize = Some(ShadowFinalize {
                    action: ShadowFinalizeAction::IntegrationBlocked,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: vec![(prepared.owner_root.clone(), reason)],
                });
                return Ok(outcome);
            }
            Err(e) => {
                // A hard failure (drift, txn write, injected crash seam): the
                // shadow stays live and a later deterministic settle resumes
                // from the durable transaction phase.
                return Err(e);
            }
        };
        // (4) The accepted contract's steps against the LANDED proof.
        outcome.steps = match self
            .run_completion_steps_against_proof(parent, &landed)
            .await
        {
            Ok(report) => report,
            Err(e) => {
                eprintln!("completion-step execution failed for shadowed run {run_id}: {e}");
                None
            }
        };
        self.check_settlement_seam(CrashSeam::BeforeTaskCompletion)?;
        let mut completed = false;
        match handle.completion_contract_gate(task_id) {
            Ok(CompletionContractGate::Satisfied) => {
                let revision = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
                match handle.complete_verified_task(task_id, revision, proof.record()) {
                    Ok(_) => completed = true,
                    Err(e) => {
                        eprintln!("shadowed completion gate refused for {run_id}: {e}");
                    }
                }
            }
            Ok(CompletionContractGate::Refused(e)) => {
                // The run stays non-terminal and a later settlement retries.
                eprintln!("shadowed run {run_id} contract gate: {e}");
            }
            Err(e) => {
                return Err(ExecError::Internal(format!(
                    "completion contract gate read for {run_id}: {e}"
                )));
            }
        }
        outcome.completed = completed;
        if completed {
            // (5) Retire the shadow ONLY after the owner holds the verified
            // candidate and the task is durably complete.
            shadows
                .mark_integrated(parent)
                .map_err(|e| ExecError::from_shadow("shadow retirement", e))?;
            outcome.complete = true;
            outcome.finalize = Some(ShadowFinalize {
                action: ShadowFinalizeAction::Integrated,
                merged: landed.prepared.changed.iter().map(PathBuf::from).collect(),
                rejected: Vec::new(),
                conflicts: Vec::new(),
            });
        } else {
            outcome.finalize = Some(retained_shadow_finalize());
        }
        Ok(outcome)
    }

    /// Recover the candidate-bound proof of an ALREADY LANDED shadow
    /// integration (the crash window between completion and shadow
    /// retirement): the landing transaction phase decides. A `Landed`
    /// transaction with the same verified candidate is finalized idempotently
    /// and a proof record is created/reused over the candidate snapshot.
    /// Everything else keeps the shadow retained.
    async fn recover_shadow_landing(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        prepared: &PreparedRunIntegration,
    ) -> Result<Option<VerifiedRunIntegration>, ExecError> {
        let Some(txn) = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        else {
            return Ok(None);
        };
        if txn.phase != faktor_session::ledger::IntegrationTxnPhase::Landed
            || txn.verified_candidate_snapshot != prepared.candidate_snapshot
        {
            return Ok(None);
        }
        // FIX 2: the task row is an explicit read — a genuinely MISSING row
        // is the not-found policy (nothing to recover), a corrupt/failed
        // read is an error, never "no criteria".
        let criteria = match handle
            .get_task(task_id)
            .map_err(|e| ExecError::from(classify_session_read("root task row read", e)))?
        {
            Some(task) => task.acceptance_criteria,
            None => return Ok(None),
        };
        let run = faktor_agent::IntegratedRootVerification {
            status: VerificationStatus::Passed,
            checks: Vec::new(),
            criteria: Vec::new(),
            changed: prepared.changed.clone(),
            summary: "already-landed shadow integration recovery".into(),
            review_model_identity: None,
        };
        let no_op_rule = prepared.changed.is_empty();
        let composed = if no_op_rule {
            compose_no_op_root_verification_status(&run.checks, &run.criteria)
        } else {
            compose_root_verification_status(&run.checks, &run.criteria)
        };
        let composed = merge_verification_status(run.status, composed);
        if composed != VerificationStatus::Passed {
            return Ok(None);
        }
        let (record, basis_digest) = self
            .find_or_create_root_verification_record(
                handle,
                task_id,
                &criteria,
                &prepared.candidate_snapshot,
                &run,
                prepared,
                composed,
            )
            .await?;
        VerifiedRunIntegration::from_composed_verdict(
            prepared.clone(),
            record,
            &run.checks,
            &run.criteria,
            no_op_rule,
            composed,
            basis_digest,
        )
        .ok_or_else(|| {
            ExecError::Internal(
                "already-landed shadow recovery could not construct its landing proof".into(),
            )
        })
        .map(Some)
    }

    /// The orchestrated arm of [`Self::settle_run`] — the contract the
    /// multi-item path was missing:
    ///
    /// 1. **aggregate/root deterministic verification**: every SPAWN item of
    ///    the durable plan must have a `Done` child; the RUN (not any
    ///    candidate) is then verified against the tournament-style derived
    ///    check set over the parent's acceptance criteria (one deterministic
    ///    check per criterion, in order) and a passing durable record for
    ///    the root task's CURRENT revision is created (reused on replay);
    /// 2. **completion steps**: the run's accepted contract executes against
    ///    the run's integration root — never a child root;
    /// 3. **completion gate**: `complete_verified_task` consumes the
    ///    aggregate record once every requested step row is `Succeeded`;
    /// 4. **finalize**: no child candidate root is ever merged/committed;
    ///    the isolated roots are returned as explicit-merge proposals only.
    ///
    /// A run that is not fully Done, or whose task row is terminal, is a
    /// deterministic no-op. Errors from the step runner are logged (the
    /// durable rows carry the typed outcome); the function itself converges.
    /// The orchestrated arm of [`Self::settle_run`] — prepare -> verify
    /// (candidate) -> land (owner) -> completion steps -> completion gate:
    ///
    /// 1. **prepare** ([`Self::prepare_run_integration`]): the run's
    ///    IMMUTABLE base is copied into a candidate root, every Done
    ///    mutating child stages its change set against the same generation,
    ///    the children are precomposed (convergent-or-conflict) and applied
    ///    to the candidate. The OWNER is never touched;
    /// 2. **verify** ([`Self::verify_prepared_integration`]): the shared
    ///    deterministic verification runs over the CANDIDATE root and a
    ///    passing record bound to the candidate snapshot is created;
    /// 3. **land** ([`Self::land_verified_integration`]): the transactional
    ///    owner landing (record-first decisions + rollback blobs, per-path
    ///    CAS applies, whole-root equality check);
    /// 4. **completion steps against the proof** and the durable completion
    ///    gate (`complete_verified_task`).
    ///
    /// A run that is not fully Done, or whose task row is terminal, is a
    /// deterministic no-op. Errors from the step runner are logged (the
    /// durable rows carry the typed outcome); preparation/landing refusals
    /// propagate typed.
    async fn settle_orchestrated(
        self: &Arc<Self>,
        parent: SessionId,
        run_id: String,
    ) -> Result<SettlementOutcome, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, &run_id)?;
        // The durable plan's SPAWN item set (the assignment contract): read
        // from the persisted plan row, never from memory. FIX 2: the plan row
        // is an explicit durable read — a MISSING row is the not-found
        // policy (nothing this settlement may name), while a PRESENT row that
        // is undecodable or lacks its `specs` is corrupt durable state and
        // refuses the settlement typed (never "no plan").
        let plan_specs: Option<Vec<ChildSpec>> = match parent_facts(&handle)?
            .into_iter()
            .find(|(kind, key, _)| kind == PLAN_ROW_KIND && key == &run_id)
        {
            None => None,
            Some((_, _, value)) => {
                let parsed: serde_json::Value = serde_json::from_str(&value).map_err(|e| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: format!("stored plan row is not decodable JSON: {e}"),
                    })
                })?;
                let specs = parsed.get("specs").cloned().ok_or_else(|| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: "stored plan row carries no `specs` array".into(),
                    })
                })?;
                let specs: Vec<ChildSpec> = serde_json::from_value(specs).map_err(|e| {
                    ExecError::from(DurableStateError::CorruptDurableState {
                        what: format!("plan row of run {run_id}"),
                        detail: format!("stored plan row `specs` is not a ChildSpec array: {e}"),
                    })
                })?;
                Some(specs)
            }
        };
        let Some(plan_specs) = plan_specs else {
            // No durable plan row: nothing this settlement may name.
            return Ok(SettlementOutcome {
                run_id: run_id.clone(),
                orchestrated: true,
                complete: false,
                verified: false,
                completed: false,
                verification: None,
                steps: None,
                merge_proposals: Vec::new(),
                finalize: None,
            });
        };
        let spawn_items: Vec<String> = plan_specs
            .into_iter()
            .filter(|s| s.spawn)
            .map(|s| s.item_id)
            .collect();
        let merge_proposals: Vec<String> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.ownership,
                    faktor_session::child::ChildOwnership::IsolatedWorktree
                        | faktor_session::child::ChildOwnership::ExclusivePaths
                )
            })
            .map(|r| r.child_id.clone())
            .collect();
        // A live shadow re-points the session's file consumers; an
        // orchestrated settlement must never land over it. (A new run is
        // already refused by `start_task`; this guards reopened/replayed
        // states.)
        if let Some(shadows) = self.shadows() {
            if let Some(row) = shadows.active_shadow(parent)? {
                if row.state.is_live() {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} carries a live shadow {}; settle the shadowed run before settling an orchestrated run",
                        row.shadow_id
                    )));
                }
            }
        }
        let finalize = None;
        // Point 8: a tournament run lands through THIS SAME pipeline once
        // its durable decision names a winner; until then it keeps the
        // explicit-merge proposal behavior (no auto integration).
        let tournament = crate::tournament::Tournament::reopen(&handle)
            .map_err(tournament_exec_error)?
            .into_iter()
            .find(|t| t.run_family == run_id);
        let tournament_decided = tournament
            .as_ref()
            .is_some_and(|t| t.state == crate::tournament::TournamentState::Decided);
        let tournament_aborted = tournament
            .as_ref()
            .is_some_and(|t| t.state == crate::tournament::TournamentState::Aborted);
        let winner: Option<String> = tournament
            .as_ref()
            .filter(|_| tournament_decided)
            .and_then(|t| t.winner.clone());
        let eligible: Vec<&super::ChildRuntime> = if let Some(winner) = &winner {
            rows.iter().filter(|r| &r.child_id == winner).collect()
        } else {
            rows.iter()
                .filter(|r| {
                    matches!(
                        r.ownership,
                        faktor_session::child::ChildOwnership::IsolatedWorktree
                            | faktor_session::child::ChildOwnership::ExclusivePaths
                    ) && spawn_items.iter().any(|i| i == &r.item_id)
                })
                .collect()
        };
        let all_done = if tournament_decided {
            !eligible.is_empty() && eligible.iter().all(|r| r.state == ChildState::Done)
        } else {
            spawn_items.iter().all(|item| {
                rows.iter()
                    .any(|r| &r.item_id == item && r.state == ChildState::Done)
            })
        };
        let mut outcome = SettlementOutcome {
            run_id: run_id.clone(),
            orchestrated: true,
            complete: all_done,
            verified: false,
            completed: false,
            verification: None,
            steps: None,
            merge_proposals,
            finalize,
        };
        let task_id = handle.task_id()?;
        // Point 7: the root task row is created unconditionally at start;
        // a legacy run whose row is missing is SEEDED here — the settlement
        // never returns early merely because no row exists.
        let task = match handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
        {
            Some(t) => t,
            None => {
                let plan = self.orchestrator.plan_row(parent, &run_id)?.plan;
                let now = handle.now_ms();
                handle
                    .create_task(faktor_session::Task {
                        task_id,
                        session_id: parent,
                        goal: truncate_bytes(&plan.goal, MAX_TASK_GOAL_BYTES),
                        acceptance_criteria: Vec::new(),
                        plan: Vec::new(),
                        attachments: Vec::new(),
                        budget: TaskBudget::default(),
                        state: TaskState::Pending,
                        created_ms: now,
                        updated_ms: now,
                    })
                    .map_err(|e| ExecError::Internal(format!("root task row seed: {e}")))?;
                handle
                    .get_task(task_id)
                    .map_err(|e| ExecError::Internal(format!("root task row re-read: {e}")))?
                    .ok_or_else(|| {
                        ExecError::Internal("root task row vanished after seeding".into())
                    })?
            }
        };
        if task.state == TaskState::VerifiedComplete {
            outcome.completed = true;
            outcome.verified = true;
            return Ok(outcome);
        }
        if !all_done || task.state.is_terminal() {
            return Ok(outcome);
        }
        if tournament.is_some() && !tournament_decided {
            // An open/aborted tournament never auto-integrates.
            if tournament_aborted {
                outcome.complete = false;
            }
            return Ok(outcome);
        }
        if !self.route_root_to_verifying(&handle, task_id)? {
            return Ok(outcome);
        }
        // The criteria are the durable row already loaded above: a state
        // transition never changes them, and a second read could only
        // degrade a vanished row to "no criteria".
        let criteria = task.acceptance_criteria.clone();
        // (1) PREPARE: compose the candidate from the immutable run base.
        // The owner is byte-untouched by this phase.
        let candidate_ids: Vec<String> = eligible.iter().map(|r| r.child_id.clone()).collect();
        let prepared =
            self.prepare_run_integration(&handle, parent, &run_id, task_id, &candidate_ids)?;
        self.check_settlement_seam(CrashSeam::AfterCandidatePrepared)?;
        // (2) VERIFY the CANDIDATE (never the owner) under the run's durable
        // no-op policy: an empty aggregate change set completes only through
        // the independent reviewer's no-op criterion proof (or the explicit
        // Allowed disposition; Refused never completes).
        let no_op = run_no_op_disposition(&handle, &run_id)?;
        let Some(proof) = self
            .verify_prepared_integration(&handle, task_id, &criteria, &prepared, no_op)
            .await?
        else {
            return Ok(outcome);
        };
        outcome.verification = Some(proof.record());
        outcome.verified = true;
        // (3) LAND the verified candidate transactionally.
        let landed = self.land_verified_integration(&handle, &proof)?;
        // (4) The accepted contract's steps against the landed proof.
        outcome.steps = match self
            .run_completion_steps_against_proof(parent, &landed)
            .await
        {
            Ok(report) => report,
            Err(e) => {
                eprintln!("completion-step execution failed for orchestrated run {run_id}: {e}");
                None
            }
        };
        self.check_settlement_seam(CrashSeam::BeforeTaskCompletion)?;
        // (5) Completion gate: the durable contract gate must be satisfied
        // (all requested step rows Succeeded) before the record is consumed.
        match handle.completion_contract_gate(task_id) {
            Ok(CompletionContractGate::Satisfied) => {
                let revision = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
                match handle.complete_verified_task(task_id, revision, proof.record()) {
                    Ok(_) => {
                        outcome.completed = true;
                    }
                    Err(e) => {
                        eprintln!(
                            "root completion gate refused for orchestrated run {run_id}: {e}"
                        );
                    }
                }
            }
            Ok(CompletionContractGate::Refused(e)) => {
                // A missing/failed/skipped step row: the run stays
                // non-terminal and a later settlement retries.
                eprintln!("orchestrated run {run_id} contract gate: {e}");
            }
            Err(e) => {
                return Err(ExecError::Internal(format!(
                    "completion contract gate read for run {run_id}: {e}"
                )));
            }
        }
        Ok(outcome)
    }

    /// PREPARE phase (point 1): build the [`PreparedRunIntegration`]
    /// candidate of one run from its IMMUTABLE base — never from the live
    /// owner. Every Done isolated child stages against the recorded run
    /// generation, the whole set is precomposed (convergent-or-conflict,
    /// order-independent) and applied once per path into the candidate.
    /// Idempotent: a replay over the same durable state rebuilds the
    /// byte-identical candidate (CAS applies are idempotent).
    pub fn prepare_run_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        parent: SessionId,
        run_id: &str,
        task_id: TaskId,
        candidate_ids: &[String],
    ) -> Result<PreparedRunIntegration, ExecError> {
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        let owner_root = self.owner_root_of(parent, handle)?;
        let rb = durable_read_value(
            format!("run base of run {run_id}"),
            handle.ledger_run_base_read(run_id),
        )
        .map_err(ExecError::from)?
        .ok_or_else(|| {
            ExecError::IntegrationConflict(format!(
                "orchestrated run {run_id} has no recorded run base; refusing to stage against the live owner"
            ))
        })?;
        let base_root = PathBuf::from(&rb.root);
        let base_snapshot = digest(&base_root)?;
        if base_snapshot != rb.snapshot_hash {
            return Err(ExecError::WorkspaceDrift(format!(
                "run base of {run_id} digests to {base_snapshot} but the durable record says {}; the generation moved",
                rb.snapshot_hash
            )));
        }
        let plan = self.orchestrator.plan_row(parent, run_id)?;
        let run_exec_dir = plan.isolated_root.join(run_id);
        std::fs::create_dir_all(&run_exec_dir)
            .map_err(|e| ExecError::Internal(format!("run exec dir {run_exec_dir:?}: {e}")))?;
        let candidate_root = run_exec_dir.join("candidate");
        std::fs::create_dir_all(&candidate_root)
            .map_err(|e| ExecError::Internal(format!("candidate root {candidate_root:?}: {e}")))?;
        let mut candidate_snapshot = digest(&candidate_root)?;
        if candidate_snapshot != rb.snapshot_hash {
            // (Re)build the candidate from the run base: remove residue,
            // copy the immutable generation, re-digest. A candidate that
            // still cannot reproduce the base is a typed drift refusal.
            std::fs::remove_dir_all(&candidate_root).map_err(|e| {
                ExecError::Internal(format!("candidate reset {}: {e}", candidate_root.display()))
            })?;
            std::fs::create_dir_all(&candidate_root).map_err(|e| {
                ExecError::Internal(format!("candidate dir {}: {e}", candidate_root.display()))
            })?;
            faktor_fs::tree_manifest::copy_tree_manifest(
                &base_root,
                &candidate_root,
                MAX_RUN_BASE_ENTRIES,
                MAX_RUN_BASE_BYTES,
                RUN_BASE_SKIP_DIRS,
            )
            .map_err(|e| {
                ExecError::WorkspaceDrift(format!(
                    "candidate run-base copy of {}: {e}",
                    base_root.display()
                ))
            })?;
            candidate_snapshot = digest(&candidate_root)?;
            if candidate_snapshot != rb.snapshot_hash {
                return Err(ExecError::WorkspaceDrift(format!(
                    "candidate of {run_id} digests to {candidate_snapshot} after copying the run base {}; refusing to verify a drifted candidate",
                    rb.snapshot_hash
                )));
            }
        }
        // Stage every eligible child against the SAME generation.
        let mut ids: Vec<String> = candidate_ids.to_vec();
        ids.sort();
        ids.dedup();
        let rows = OrchestratorRuntime::registry_rows(self.session.clone(), parent, run_id)?;
        let mut staged_sets: Vec<crate::runtime::merge::ChangeSet> = Vec::new();
        let mut staged: Vec<PreparedChildChangeSet> = Vec::new();
        let mut sources: Vec<faktor_session::IntegrationSourceRow> = Vec::new();
        for child_id in &ids {
            let child = rows
                .iter()
                .find(|r| &r.child_id == child_id)
                .ok_or_else(|| ExecError::NotFound(format!("child {child_id}")))?;
            if child.state != ChildState::Done {
                return Err(ExecError::InvalidState(format!(
                    "cannot integrate non-Done isolated child {} (state {:?})",
                    child.child_id, child.state
                )));
            }
            if !matches!(
                child.ownership,
                faktor_session::child::ChildOwnership::IsolatedWorktree
                    | faktor_session::child::ChildOwnership::ExclusivePaths
            ) {
                continue;
            }
            let cs = self.orchestrator.stage_child_changes(child_id)?;
            let child_root = self.orchestrator.child_worktree_dir(child)?;
            let candidate_root_hash = digest(&child_root)?;
            sources.push(faktor_session::IntegrationSourceRow {
                child_id: child.child_id.clone(),
                change_set_id: cs.id(),
                candidate_root_hash: candidate_root_hash.clone(),
            });
            staged_sets.push(cs.clone());
            staged.push(PreparedChildChangeSet {
                child_id: child.child_id.clone(),
                child_root,
                change_set: cs,
                candidate_root_hash,
            });
        }
        sources.sort_by(|a, b| a.child_id.cmp(&b.child_id));
        // Precompose: convergent children apply ONCE; any divergence (or
        // delete-vs-modify) is a typed conflict before any owner mutation.
        let composed = crate::runtime::merge::compose_child_changes(&staged_sets)?;
        for change in &composed {
            self.apply_composed_path_to_candidate(&candidate_root, &staged, change)?;
        }
        candidate_snapshot = digest(&candidate_root)?;
        let changed: Vec<String> = composed
            .iter()
            .map(|c| c.path.to_string_lossy().into_owned())
            .collect();
        let sources_digest = if sources.is_empty() {
            String::new()
        } else {
            stable_list_digest(
                &sources
                    .iter()
                    .map(|s| {
                        format!(
                            "{}|{}|{}",
                            s.child_id, s.change_set_id, s.candidate_root_hash
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        };
        Ok(PreparedRunIntegration {
            run_id: run_id.to_string(),
            task_id,
            owner_root,
            base_root,
            candidate_root,
            base_snapshot,
            candidate_snapshot,
            changed,
            sources,
            sources_digest,
            staged,
        })
    }

    /// Apply ONE composed per-path decision into the candidate root, using
    /// the deterministic representative child for the source content.
    fn apply_composed_path_to_candidate(
        &self,
        candidate_root: &std::path::Path,
        staged: &[PreparedChildChangeSet],
        change: &crate::runtime::merge::ComposedPathChange,
    ) -> Result<(), ExecError> {
        match &change.child {
            Some(candidate) => {
                let source = staged
                    .iter()
                    .filter(|s| change.sources.contains(&s.child_id))
                    .min_by(|a, b| a.child_id.cmp(&b.child_id))
                    .ok_or_else(|| {
                        ExecError::Internal(format!(
                            "composed path {:?} names no staged source",
                            change.path
                        ))
                    })?;
                crate::runtime::merge::apply_manifest_entry(
                    candidate_root,
                    &change.path,
                    &source.child_root,
                    &change.path,
                    candidate,
                    change.base.as_ref(),
                )
                .map_err(|e| {
                    ExecError::from_fs(
                        &format!("candidate composition of {:?}", change.path),
                        candidate_root,
                        e,
                    )
                })?;
            }
            None => {
                let base = change.base.as_ref().ok_or_else(|| {
                    ExecError::Internal(format!(
                        "composed deletion {:?} has no base anchor",
                        change.path
                    ))
                })?;
                crate::runtime::merge::delete_manifest_entry(candidate_root, &change.path, base)
                    .map_err(|e| {
                        ExecError::from_fs(
                            &format!("candidate composition of {:?}", change.path),
                            candidate_root,
                            e,
                        )
                    })?;
            }
        }
        Ok(())
    }

    /// PREPARE phase of one single-item SHADOW run (the one-child instance
    /// of [`Self::prepare_run_integration`]): the shadow's staged change set
    /// is bound to the shadow's IMMUTABLE run base (`RunBaseRecord` recorded
    /// at `begin_shadow` under the shadow id) and the shadow tree is the
    /// CANDIDATE (base + the one child's changes, verified byte-identical to
    /// the staged child content). The owner is never a staging source; the
    /// OWNER is only the materialized base under the whole-root equality
    /// check every landing performs first, and each per-path rollback blob
    /// is re-verified against its recorded anchor before the transaction is
    /// recorded (see [`Self::build_path_decisions`]).
    pub fn prepare_shadow_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        parent: SessionId,
        run_id: &str,
        task_id: TaskId,
        row: &ShadowRow,
    ) -> Result<PreparedRunIntegration, ExecError> {
        let shadows = self.shadows().ok_or_else(|| {
            ExecError::Internal("shadow integration requires the shadow service".into())
        })?;
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        let owner_root = PathBuf::from(&row.base_root).canonicalize().map_err(|e| {
            ExecError::NotFound(format!(
                "shadow integration target {} vanished since begin_shadow: {e}",
                row.base_root
            ))
        })?;
        let candidate_root = PathBuf::from(&row.root).canonicalize().map_err(|e| {
            ExecError::NotFound(format!(
                "shadow root {} is gone; discard() and begin_shadow() again: {e}",
                row.root
            ))
        })?;
        let run_base = durable_read_value(
            format!("run base of shadow {}", row.shadow_id),
            handle.ledger_run_base_read(&row.shadow_id),
        )
        .map_err(ExecError::from)?
        .ok_or_else(|| {
            ExecError::IntegrationConflict(format!(
                "shadow {} has no recorded run base; refusing to integrate an unanchored candidate",
                row.shadow_id
            ))
        })?;
        let cs = shadows.stage_change_set(parent)?;
        if cs.run_base_snapshot.as_deref() != Some(run_base.snapshot_hash.as_str()) {
            return Err(ExecError::IntegrationConflict(format!(
                "change set {} of shadow {} binds run base {:?} but the shadow carries {}; a stale change set is never integrated",
                cs.id(),
                row.shadow_id,
                cs.run_base_snapshot,
                run_base.snapshot_hash
            )));
        }
        let candidate_snapshot = digest(&candidate_root)?;
        let changed: Vec<String> = cs
            .files
            .iter()
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect();
        let source = faktor_session::IntegrationSourceRow {
            child_id: "shadow".to_string(),
            change_set_id: cs.id(),
            candidate_root_hash: candidate_snapshot.clone(),
        };
        let sources_digest =
            stable_list_digest(&[format!("shadow|{}|{}", cs.id(), candidate_snapshot)]);
        let staged = vec![PreparedChildChangeSet {
            child_id: "shadow".to_string(),
            child_root: candidate_root.clone(),
            change_set: cs,
            candidate_root_hash: candidate_snapshot.clone(),
        }];
        Ok(PreparedRunIntegration {
            run_id: run_id.to_string(),
            task_id,
            owner_root: owner_root.clone(),
            // Materialized base: at decision time the owner root is proven
            // byte-identical to the recorded run base before the first
            // durable decision row or write, and every blob read is
            // re-verified against its recorded base hash.
            base_root: owner_root,
            candidate_root,
            base_snapshot: run_base.snapshot_hash,
            candidate_snapshot,
            changed,
            sources: vec![source],
            sources_digest,
            staged,
        })
    }

    /// VERIFY phase (points 1/2/6): run the shared deterministic
    /// verification over the CANDIDATE root, persist the durable fact and a
    /// verification record bound to the CANDIDATE snapshot. `Ok(None)` for
    /// a failed/unavailable run (completion stays refused); a passing run
    /// returns the proof the landing phase consumes.
    ///
    /// The P0 no-op policy of an EMPTY aggregate change set is applied HERE:
    /// `Refused` never verifies (no record, completion stays refused);
    /// `Allowed` (an explicitly investigative task) mints the empty no-op
    /// record without a criterion proof; `RequiresCriterionProof` routes
    /// through [`faktor_agent::AgentRuntime::verify_integrated_root`]'s
    /// reviewer-proved no-op path — no reviewer verdict is a typed refusal,
    /// never a synthetic pass over an empty suite.
    pub async fn verify_prepared_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        criteria: &[String],
        prepared: &PreparedRunIntegration,
        no_op: NoOpDisposition,
    ) -> Result<Option<VerifiedRunIntegration>, ExecError> {
        let token = CancellationToken::new();
        let run = if prepared.changed.is_empty() && no_op == NoOpDisposition::Refused {
            persist_root_verification_fact(handle, "pending", &[], &[])?;
            eprintln!(
                "orchestrated run {} produced an empty aggregate change set and its no-op policy is refused; completion stays blocked",
                prepared.run_id
            );
            return Ok(None);
        } else if prepared.changed.is_empty() && no_op == NoOpDisposition::Allowed {
            faktor_agent::IntegratedRootVerification {
                status: VerificationStatus::Passed,
                checks: Vec::new(),
                criteria: Vec::new(),
                changed: Vec::new(),
                summary:
                    "empty aggregate change set allowed without criterion proof by the run's no-op policy"
                        .into(),
                // An `Allowed` no-op is an explicitly investigative task and
                // dispatches NO review-model call: no reviewer identity may
                // be claimed.
                review_model_identity: None,
            }
        } else if self
            .orchestrator
            .agent()
            .deps()
            .verification
            .can_persist_jobs()
        {
            // Attempt-based root verification (P0-5/26 production wiring):
            // cheap checks run inline, expensive checks are enqueued as
            // durable jobs, and the run consumes the EXACT attempt recorded
            // under its durable `verification_attempt/root:{run}` fact. A
            // pending attempt leaves the run unverified — the daemon
            // verification executor (or a later settlement) resolves it, and
            // only THAT attempt may certify (a newer attempt supersedes the
            // old one structurally; late results are typed-refused).
            let attempt_op = match read_root_attempt_op(handle, &prepared.run_id)? {
                Some(op) => op,
                None => {
                    let op = self.session.next_op_id().raw();
                    persist_root_attempt_op(handle, &prepared.run_id, op)?;
                    op
                }
            };
            match self
                .orchestrator
                .agent()
                .verify_integrated_root_attempt(
                    handle,
                    &prepared.candidate_root,
                    &prepared.changed,
                    criteria,
                    &token,
                    attempt_op,
                )
                .await
            {
                Ok(outcome) => {
                    if outcome.attempt_op != attempt_op {
                        // A superseded attempt was replaced by a fresh one:
                        // the run now consumes the FRESH attempt only.
                        persist_root_attempt_op(handle, &prepared.run_id, outcome.attempt_op)?;
                    }
                    if outcome.pending {
                        persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                        eprintln!(
                            "root verification attempt {} of orchestrated run {} is still executing; completion waits",
                            outcome.attempt_op, prepared.run_id
                        );
                        return Ok(None);
                    }
                    match outcome.verification {
                        Some(run) => run,
                        None => {
                            persist_root_verification_fact(
                                handle,
                                "pending",
                                &[],
                                &prepared.changed,
                            )?;
                            return Ok(None);
                        }
                    }
                }
                Err(e) => {
                    persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                    eprintln!(
                        "root verification unavailable for orchestrated run {}: {e}; run stays unverified",
                        prepared.run_id
                    );
                    return Ok(None);
                }
            }
        } else {
            match self
                .orchestrator
                .agent()
                .verify_integrated_root(
                    handle,
                    &prepared.candidate_root,
                    &prepared.changed,
                    criteria,
                    &token,
                )
                .await
            {
                Ok(run) => run,
                Err(e) => {
                    persist_root_verification_fact(handle, "pending", &[], &prepared.changed)?;
                    eprintln!(
                        "root verification unavailable for orchestrated run {}: {e}; run stays unverified",
                        prepared.run_id
                    );
                    return Ok(None);
                }
            }
        };
        // The landing AUTHORITY is the COMPOSED verdict, never the run's
        // check-side status alone: green checks never authorize landing over
        // a failed or unavailable required criterion. The agent's check-side
        // status is folded in because it also witnesses required checks that
        // produced no execution row at all (missing evidence, never a pass).
        let no_op_rule = prepared.changed.is_empty();
        let composed = if no_op_rule {
            compose_no_op_root_verification_status(&run.checks, &run.criteria)
        } else {
            compose_root_verification_status(&run.checks, &run.criteria)
        };
        let composed = merge_verification_status(run.status, composed);
        let (record, basis_digest) = self
            .find_or_create_root_verification_record(
                handle,
                task_id,
                criteria,
                &prepared.candidate_snapshot,
                &run,
                prepared,
                composed,
            )
            .await?;
        persist_root_verification_fact(
            handle,
            verification_status_tag(composed),
            &run.checks,
            &prepared.changed,
        )?;
        if composed != VerificationStatus::Passed {
            return Ok(None);
        }
        self.check_settlement_seam(CrashSeam::AfterPreparedVerification)?;
        let proof = VerifiedRunIntegration::from_composed_verdict(
            prepared.clone(),
            record,
            &run.checks,
            &run.criteria,
            no_op_rule,
            composed,
            basis_digest,
        )
        .ok_or_else(|| {
            ExecError::Internal(format!(
                "composed verdict of run {} is Passed but no landing proof could be constructed",
                prepared.run_id
            ))
        })?;
        Ok(Some(proof))
    }

    /// LAND phase (points 5/6): transactionally apply the VERIFIED
    /// candidate into the owner. Record-first decisions + rollback blobs,
    /// owner-equals-run-base recheck, per-path CAS applies, fresh whole-root
    /// equality against the verified candidate, then the finalized
    /// integration record. Never called before a passing verification.
    pub fn land_verified_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
    ) -> Result<LandedRunIntegration, ExecError> {
        // The constructor only ever yields a proof whose COMPOSED verdict is
        // `Passed`; re-assert it here so a future refactor can never reach
        // the first owner write with a non-passing composed status.
        if proof.composed_status() != VerificationStatus::Passed {
            return Err(ExecError::Internal(format!(
                "landing refused: run {} carries a non-passing ({:?}) composed root verdict",
                proof.prepared().run_id,
                proof.composed_status()
            )));
        }
        let prepared = proof.prepared();
        let owner_root = &prepared.owner_root;
        let digest =
            |root: &std::path::Path| -> Result<String, ExecError> { root_manifest_digest(root) };
        // Crash recovery: inspect the durable transaction of this run and
        // deterministically finish landing or roll back.
        if let Some(mut txn) = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        {
            let same_candidate = txn.verified_candidate_snapshot == prepared.candidate_snapshot;
            match txn.phase {
                faktor_session::ledger::IntegrationTxnPhase::Landed if same_candidate => {
                    let landed = digest(owner_root)?;
                    if landed != prepared.candidate_snapshot {
                        return Err(ExecError::WorkspaceDrift(format!(
                            "owner root {} digests to {landed} but the Landed transaction binds {}",
                            owner_root.display(),
                            prepared.candidate_snapshot
                        )));
                    }
                    self.finalize_integration_record(
                        handle,
                        prepared,
                        &landed,
                        proof.proof_basis_digest(),
                    )?;
                    return Ok(LandedRunIntegration {
                        prepared: prepared.clone(),
                        record: proof.record(),
                        landed_snapshot: landed,
                    });
                }
                faktor_session::ledger::IntegrationTxnPhase::Landing if same_candidate => {
                    return self.resume_landing(handle, proof, &mut txn);
                }
                faktor_session::ledger::IntegrationTxnPhase::RollingBack if same_candidate => {
                    self.rollback_integration(handle, &mut txn)?;
                    return Err(ExecError::IntegrationConflict(format!(
                        "integration transaction of run {} was rolled back; the run must re-settle",
                        prepared.run_id
                    )));
                }
                faktor_session::ledger::IntegrationTxnPhase::RolledBack if same_candidate => {
                    // The rolled-back attempt is finished; fall through to a
                    // FRESH attempt (owner may have been restored).
                }
                _ => {}
            }
        }
        // Fresh attempt: the WHOLE owner root must still equal the run base
        // BEFORE the first durable decision row or owner write.
        let owner_now = digest(owner_root)?;
        if owner_now != prepared.base_snapshot {
            let reason = format!(
                "owner root {} digests to {owner_now} but the run base is {}; a drifted owner blocks landing before any write",
                owner_root.display(),
                prepared.base_snapshot
            );
            self.record_blocked_txn(handle, prepared, &reason)?;
            return Err(ExecError::IntegrationConflict(reason));
        }
        let decisions = self.build_path_decisions(prepared)?;
        let mut txn = faktor_session::ledger::IntegrationTxnRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            owner_root: owner_root.to_string_lossy().into_owned(),
            candidate_root: prepared.candidate_root.to_string_lossy().into_owned(),
            run_base_snapshot: prepared.base_snapshot.clone(),
            verified_candidate_snapshot: prepared.candidate_snapshot.clone(),
            sources_digest: prepared.sources_digest.clone(),
            phase: faktor_session::ledger::IntegrationTxnPhase::Prepared,
            paths: decisions,
            path_count: prepared.changed.len() as u64,
            applied_count: 0,
            conflicts: Vec::new(),
            at_ms: handle.now_ms(),
        };
        // Record-first: every decision + rollback blob (already in the CAS)
        // is durable BEFORE any owner write.
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Verified;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        // Recheck the owner under the durable decisions; a drift here still
        // lands nothing.
        let owner_now = digest(owner_root)?;
        if owner_now != prepared.base_snapshot {
            let reason = format!(
                "owner root {} moved to {owner_now} after the landing decisions were recorded (run base {})",
                owner_root.display(),
                prepared.base_snapshot
            );
            txn.phase = faktor_session::ledger::IntegrationTxnPhase::Blocked;
            txn.conflicts.push(truncate_bytes(&reason, 256));
            txn.at_ms = handle.now_ms();
            handle
                .ledger_integration_txn_set(&txn)
                .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
            return Err(ExecError::IntegrationConflict(reason));
        }
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Landing;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(&txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        self.check_settlement_seam(CrashSeam::AfterIntegrationTxnRecord)?;
        self.apply_landing_loop(handle, proof, &mut txn)
    }

    /// Finish a durable `Landing` transaction: re-apply every Pending path
    /// (CAS-idempotent) and reconcile a conflict into a rollback.
    fn resume_landing(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<LandedRunIntegration, ExecError> {
        self.apply_landing_loop(handle, proof, txn)
    }

    /// The per-path apply loop of a landing transaction. Every applied path
    /// is journaled immediately; a conflict enters the rollback path (the
    /// owner is never left half-landed without a durable recovery row).
    fn apply_landing_loop(
        &self,
        handle: &faktor_session::SessionHandle,
        proof: &VerifiedRunIntegration,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<LandedRunIntegration, ExecError> {
        let prepared = proof.prepared();
        let owner_root = PathBuf::from(&txn.owner_root);
        for i in 0..txn.paths.len() {
            if txn.paths[i].state != faktor_session::ledger::IntegrationPathTxnState::Pending {
                continue;
            }
            let path_txn = txn.paths[i].clone();
            match self.apply_landing_path(&owner_root, &prepared.candidate_root, &path_txn) {
                Ok(()) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::Applied;
                    txn.applied_count = txn
                        .paths
                        .iter()
                        .filter(|p| {
                            p.state == faktor_session::ledger::IntegrationPathTxnState::Applied
                        })
                        .count() as u64;
                    txn.at_ms = handle.now_ms();
                    handle
                        .ledger_integration_txn_set(txn)
                        .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
                }
                Err(detail) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::Conflict;
                    let line = format!("{}: {detail}", txn.paths[i].path);
                    txn.conflicts.push(truncate_bytes(&line, 256));
                    txn.at_ms = handle.now_ms();
                    handle
                        .ledger_integration_txn_set(txn)
                        .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
                    self.record_blocked_integration(handle, prepared, &txn.conflicts)?;
                    self.rollback_integration(handle, txn)?;
                    return Err(ExecError::IntegrationConflict(format!(
                        "owner landing of run {} conflicted at {}; the applied paths were rolled back",
                        prepared.run_id, txn.paths[i].path
                    )));
                }
            }
            self.check_settlement_seam(CrashSeam::IntegrationApply { after: i + 1 })?;
        }
        // Final equality (point 6): the landed owner root must EXACTLY
        // equal the verified candidate snapshot — never "merge returned ok".
        let landed = root_manifest_digest(&owner_root).map_err(|e| {
            ExecError::WorkspaceDrift(format!(
                "landed root snapshot of {}: {e}",
                owner_root.display()
            ))
        })?;
        if landed != prepared.candidate_snapshot {
            let reason = format!(
                "landed owner root {} digests to {landed} but the verified candidate is {}",
                owner_root.display(),
                prepared.candidate_snapshot
            );
            self.rollback_integration(handle, txn)?;
            txn.conflicts.push(truncate_bytes(&reason, 256));
            return Err(ExecError::WorkspaceDrift(reason));
        }
        self.check_settlement_seam(CrashSeam::AfterFinalOwnerSnapshot)?;
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::Landed;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        self.finalize_integration_record(handle, prepared, &landed, proof.proof_basis_digest())?;
        Ok(LandedRunIntegration {
            prepared: prepared.clone(),
            record: proof.record(),
            landed_snapshot: landed,
        })
    }

    /// Apply ONE per-path landing decision to the owner through the canonical
    /// entry-state CAS primitive: the owner's live `(kind, mode, payload /
    /// literal target)` must still equal the recorded base state, the
    /// candidate bytes are re-verified against the recorded candidate state,
    /// and the whole kind/mode/content/target transition is atomic. A legacy
    /// (byte-only) decision row is a typed refusal — never landed from.
    fn apply_landing_path(
        &self,
        owner_root: &std::path::Path,
        candidate_root: &std::path::Path,
        path_txn: &faktor_session::ledger::IntegrationPathTxn,
    ) -> Result<(), String> {
        use faktor_fs::entry_state::EntryState;
        if !path_txn.canonical_ready() {
            return Err(format!(
                "landing decision of {:?} is not a canonical entry-state row (legacy byte-only material); refusing to land it",
                path_txn.path
            ));
        }
        let rel = std::path::Path::new(&path_txn.path);
        if path_txn.candidate_state == EntryState::Absent {
            crate::runtime::merge::delete_manifest_entry(owner_root, rel, &path_txn.base_state)
                .map(|_| ())
                .map_err(|e| e.message)
        } else {
            let payload = crate::runtime::merge::read_literal_payload(
                candidate_root,
                rel,
                &path_txn.candidate_state,
            )
            .map_err(|e| e.message)?;
            if !path_txn.candidate_state.material_matches(&payload) {
                return Err(format!(
                    "candidate {} drifted since staging; nothing was landed",
                    path_txn.path
                ));
            }
            faktor_fs::entry_state::apply_tree_entry_cas(
                owner_root,
                rel,
                &path_txn.base_state,
                &path_txn.candidate_state,
                &payload,
            )
            .map(|_| ())
            .map_err(|e| e.message)
        }
    }

    /// Build the record-first per-path decisions of a fresh landing in the
    /// CANONICAL entry-state vocabulary: `base_state` + `candidate_state`
    /// (kind/mode/payload / literal target) and the rollback CAS material
    /// (the base regular payload blob in the CAS, the base symlink literal
    /// target inline). A base path that is a special file the canonical
    /// vocabulary cannot represent is a typed refusal before anything is
    /// recorded.
    fn build_path_decisions(
        &self,
        prepared: &PreparedRunIntegration,
    ) -> Result<Vec<faktor_session::ledger::IntegrationPathTxn>, ExecError> {
        use faktor_fs::entry_state::EntryState;
        let base_rows = crate::runtime::merge::canonical_manifest_rows(
            &prepared.base_root,
            MAX_RUN_BASE_ENTRIES,
        )?;
        let candidate_rows = crate::runtime::merge::canonical_manifest_rows(
            &prepared.candidate_root,
            MAX_RUN_BASE_ENTRIES,
        )?;
        let base_map: std::collections::HashMap<PathBuf, EntryState> =
            base_rows.into_iter().collect();
        let candidate_map: std::collections::HashMap<PathBuf, EntryState> =
            candidate_rows.into_iter().collect();
        let mut decisions = Vec::with_capacity(prepared.changed.len());
        for rel in &prepared.changed {
            let path = PathBuf::from(rel);
            let base_state = base_map.get(&path).cloned().unwrap_or(EntryState::Absent);
            let candidate_state = candidate_map
                .get(&path)
                .cloned()
                .unwrap_or(EntryState::Absent);
            let (rollback_blob, rollback_link_target) = match &base_state {
                EntryState::Absent => (None, None),
                EntryState::Regular { payload, .. } => {
                    let bytes = crate::runtime::merge::read_literal_payload(
                        &prepared.base_root,
                        &path,
                        &base_state,
                    )
                    .map_err(|e| {
                        ExecError::WorkspaceDrift(format!(
                            "run base content of {rel} no longer matches its recorded state: {e}"
                        ))
                    })?;
                    let stored = self
                        .session
                        .cas()
                        .put_bounded(&bytes, faktor_fs::MAX_MERGE_FILE_BYTES as usize)
                        .map_err(|e| ExecError::Internal(format!("run base blob of {rel}: {e}")))?;
                    // The materialized base must still match the recorded
                    // anchor: a drifted generation is a typed refusal, never
                    // a rollback blob for the wrong content.
                    if stored != *payload {
                        return Err(ExecError::WorkspaceDrift(format!(
                            "run base content of {rel} no longer matches its recorded anchor (expected {}, found {}); the generation moved",
                            payload.to_hex(),
                            stored.to_hex()
                        )));
                    }
                    (Some(stored.to_hex()), None)
                }
                EntryState::Symlink { target, .. } => (None, Some(target.clone())),
            };
            let decision = faktor_session::ledger::IntegrationPathTxn {
                path: rel.clone(),
                base_state,
                candidate_state,
                rollback_blob,
                rollback_link_target,
                canonical: true,
                state: faktor_session::ledger::IntegrationPathTxnState::Pending,
            };
            if !decision.canonical_ready() {
                return Err(ExecError::Internal(format!(
                    "landing decision of {rel} is not canonical-ready after construction"
                )));
            }
            decisions.push(decision);
        }
        Ok(decisions)
    }

    /// Roll back every Applied path of a landing transaction: restore the
    /// base content ONLY while the owner path still holds OUR written
    /// candidate state; a later user edit is never overwritten (the path is
    /// marked [`faktor_session::ledger::IntegrationPathTxnState::RollbackConflict`]).
    fn rollback_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        txn: &mut faktor_session::ledger::IntegrationTxnRow,
    ) -> Result<(), ExecError> {
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::RollingBack;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        let owner_root = PathBuf::from(&txn.owner_root);
        for i in (0..txn.paths.len()).rev() {
            if txn.paths[i].state != faktor_session::ledger::IntegrationPathTxnState::Applied {
                continue;
            }
            let path_txn = txn.paths[i].clone();
            match self.restore_base_path(&owner_root, &path_txn) {
                Ok(()) => {
                    txn.paths[i].state = faktor_session::ledger::IntegrationPathTxnState::RolledBack
                }
                Err(detail) => {
                    txn.paths[i].state =
                        faktor_session::ledger::IntegrationPathTxnState::RollbackConflict;
                    let line = format!("{}: {detail}", path_txn.path);
                    txn.conflicts.push(truncate_bytes(&line, 256));
                }
            }
            txn.at_ms = handle.now_ms();
            handle
                .ledger_integration_txn_set(txn)
                .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
            self.check_settlement_seam(CrashSeam::DuringRollback)?;
        }
        txn.phase = faktor_session::ledger::IntegrationTxnPhase::RolledBack;
        txn.at_ms = handle.now_ms();
        handle
            .ledger_integration_txn_set(txn)
            .map_err(|e| ExecError::Internal(format!("integration txn write: {e}")))?;
        Ok(())
    }

    /// Restore ONE applied path to its exact base state through the
    /// canonical restore CAS: the live state must still be the transaction's
    /// written candidate state (a later user edit is a typed Conflict and is
    /// preserved), and the exact kind/mode/target is restored from the
    /// recorded rollback material.
    fn restore_base_path(
        &self,
        owner_root: &std::path::Path,
        path_txn: &faktor_session::ledger::IntegrationPathTxn,
    ) -> Result<(), String> {
        use faktor_fs::entry_state::EntryState;
        if !path_txn.canonical_ready() {
            return Err(format!(
                "rollback decision of {:?} is not a canonical entry-state row; refusing to restore from it",
                path_txn.path
            ));
        }
        let rel = std::path::Path::new(&path_txn.path);
        let material = match &path_txn.base_state {
            EntryState::Absent => Vec::new(),
            EntryState::Regular { payload, .. } => {
                let blob = path_txn
                    .rollback_blob
                    .as_deref()
                    .ok_or_else(|| "regular rollback has no base blob".to_string())?;
                let hash = FileHash::from_hex(blob)
                    .ok_or_else(|| format!("hostile rollback blob {blob:?}"))?;
                if hash != *payload {
                    return Err(
                        "rollback blob digest does not match the recorded base state".to_string(),
                    );
                }
                self.session
                    .cas()
                    .get_bounded(hash, faktor_fs::MAX_MERGE_FILE_BYTES as usize)
                    .map_err(|e| format!("rollback blob read: {e}"))?
                    .ok_or_else(|| "rollback blob missing from the CAS".to_string())?
            }
            EntryState::Symlink { target, .. } => target.clone(),
        };
        faktor_fs::entry_state::restore_tree_entry_cas(
            owner_root,
            rel,
            &path_txn.candidate_state,
            &path_txn.base_state,
            &material,
        )
        .map(|_| ())
        .map_err(|e| e.message)
    }

    /// Record a Blocked landing attempt (owner drift before the first
    /// write): a durable in-flight integration record with the typed reason
    /// and the staged sources, so nothing about the run is lost.
    fn record_blocked_txn(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        reason: &str,
    ) -> Result<(), ExecError> {
        self.record_blocked_integration(handle, prepared, &[truncate_bytes(reason, 256)])
    }

    fn record_blocked_integration(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        conflicts: &[String],
    ) -> Result<(), ExecError> {
        let files_digest = if prepared.changed.is_empty() {
            String::new()
        } else {
            stable_list_digest(&prepared.changed)
        };
        // A blocked record names its landing transaction when one exists
        // (the deterministic txn identity), and the exact run base /
        // candidate it refused to land. NOTHING is derived from
        // `sources.first()` anymore: the deprecated `base_revision` stays
        // `None`.
        let integration_txn_id = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        .map(|txn| txn.txn_id());
        let row = faktor_session::IntegrationRecordRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            base_revision: None,
            base_snapshot: Some(prepared.base_snapshot.clone()),
            run_base_snapshot: Some(prepared.base_snapshot.clone()),
            candidate_snapshot: Some(prepared.candidate_snapshot.clone()),
            landed_snapshot: None,
            proof_basis_digest: None,
            integration_txn_id,
            final_root: prepared.owner_root.to_string_lossy().into_owned(),
            final_snapshot_hash: String::new(),
            integrated_files: prepared
                .changed
                .iter()
                .take(faktor_session::MAX_INTEGRATION_FILES)
                .cloned()
                .collect(),
            integrated_file_count: prepared.changed.len() as u64,
            integrated_files_digest: files_digest,
            conflicts: conflicts
                .iter()
                .take(faktor_session::MAX_INTEGRATION_CONFLICTS)
                .cloned()
                .collect(),
            conflict_count: conflicts.len() as u64,
            sources: prepared
                .sources
                .iter()
                .take(faktor_session::MAX_INTEGRATION_SOURCES)
                .cloned()
                .collect(),
            source_count: prepared.sources.len() as u64,
            sources_digest: prepared.sources_digest.clone(),
            at_ms: handle.now_ms(),
        };
        handle
            .ledger_integration_record_set(&row)
            .map(|_| ())
            .map_err(|e| ExecError::Internal(format!("integration record write: {e}")))
    }

    /// Finalize the integration record with the FRESH whole-root digest
    /// taken after the landing (point 6). Idempotent for an already
    /// finalized identical landing.
    fn finalize_integration_record(
        &self,
        handle: &faktor_session::SessionHandle,
        prepared: &PreparedRunIntegration,
        landed: &str,
        proof_basis_digest: &str,
    ) -> Result<(), ExecError> {
        if let Some(existing) = handle
            .ledger_integration_record_for_task(prepared.task_id.raw())
            .map_err(|e| ExecError::Internal(format!("integration record read: {e}")))?
        {
            if existing.final_snapshot_hash == landed
                && existing.sources_digest == prepared.sources_digest
                && existing.run_id == prepared.run_id
            {
                return Ok(());
            }
        }
        let files_digest = if prepared.changed.is_empty() {
            String::new()
        } else {
            stable_list_digest(&prepared.changed)
        };
        // The deterministic identity of the landing transaction that
        // produced this snapshot (record-first rows always exist by the
        // time an integration is finalized; a `None` here is only possible
        // for a direct caller that never landed through a txn).
        let integration_txn_id = durable_read_value(
            format!("landing transaction of run {}", prepared.run_id),
            handle.ledger_integration_txn_read(&prepared.run_id),
        )
        .map_err(ExecError::from)?
        .map(|txn| txn.txn_id());
        let row = faktor_session::IntegrationRecordRow {
            run_id: prepared.run_id.clone(),
            task_id: prepared.task_id.raw(),
            // The deprecated overloaded field is never derived or populated
            // (the explicit identity fields below carry the real values).
            base_revision: None,
            base_snapshot: Some(prepared.base_snapshot.clone()),
            run_base_snapshot: Some(prepared.base_snapshot.clone()),
            candidate_snapshot: Some(prepared.candidate_snapshot.clone()),
            landed_snapshot: Some(landed.to_string()),
            proof_basis_digest: Some(proof_basis_digest.to_string()),
            integration_txn_id,
            final_root: prepared.owner_root.to_string_lossy().into_owned(),
            final_snapshot_hash: landed.to_string(),
            integrated_files: prepared
                .changed
                .iter()
                .take(faktor_session::MAX_INTEGRATION_FILES)
                .cloned()
                .collect(),
            integrated_file_count: prepared.changed.len() as u64,
            integrated_files_digest: files_digest,
            conflicts: Vec::new(),
            conflict_count: 0,
            sources: prepared
                .sources
                .iter()
                .take(faktor_session::MAX_INTEGRATION_SOURCES)
                .cloned()
                .collect(),
            source_count: prepared.sources.len() as u64,
            sources_digest: prepared.sources_digest.clone(),
            at_ms: handle.now_ms(),
        };
        handle
            .ledger_integration_record_set(&row)
            .map(|_| ())
            .map_err(|e| ExecError::Internal(format!("integration record write: {e}")))
    }

    /// Find-or-create the root verification record of one orchestrated run at
    /// the CURRENT revision, bound to the final integration snapshot
    /// (`tree_hash`), covering every acceptance criterion and carrying the
    /// COMPOSED verdict `status` (never the check-side status alone). A
    /// matching record is reused ONLY while its persisted proof basis is
    /// byte-identical to the current basis
    /// ([`SessionHandle::verification_record_reusable`]); a record with a
    /// different basis (a changed candidate snapshot, check set, task contract
    /// or revision) — or a legacy record without a basis — is NEVER reused
    /// and a fresh record is written. The fresh record lands WITH its
    /// environment fingerprint (carrying the proof-basis digest) and its
    /// candidate-proof reference, so the next settlement can consult the
    /// basis of a record this call created.
    #[allow(clippy::too_many_arguments)]
    async fn find_or_create_root_verification_record(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        criteria: &[String],
        final_snapshot: &str,
        run: &faktor_agent::IntegratedRootVerification,
        prepared: &PreparedRunIntegration,
        status: VerificationStatus,
    ) -> Result<(VerificationRecordId, String), ExecError> {
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
        let covers = |r: &faktor_session::VerificationRecord| {
            criteria.iter().all(|c| {
                r.criteria
                    .iter()
                    .any(|cv| cv.criterion_key == *c && cv.passed)
            })
        };
        let basis = self
            .root_verification_proof_basis(handle, task_id, prepared, run)
            .await?;
        let basis_digest = canonical_proof_basis_digest(&basis);
        let mut candidates: Vec<faktor_session::VerificationRecord> = handle
            .list_verification_records(task_id)
            .map_err(|e| ExecError::Internal(format!("verification record list: {e}")))?
            .into_iter()
            .filter(|r| {
                r.status == status
                    && r.revision == revision
                    && r.tree_hash.as_deref() == Some(final_snapshot)
                    && (status != VerificationStatus::Passed || covers(r))
            })
            .collect();
        // Newest first: a newer basis-bound record is preferred, and a
        // divergent older record can never shadow it.
        candidates.sort_by_key(|r| std::cmp::Reverse(r.record_id));
        for existing in &candidates {
            match canonical_proof_reuse(handle, existing.record_id, &basis_digest)? {
                ProofReuse::Allowed => return Ok((existing.record_id, basis_digest)),
                ProofReuse::Refused { reason } => {
                    eprintln!(
                        "root verification record {} of task {task_id} is not reusable: {reason}; consulting older records / writing a fresh basis-bound record",
                        existing.record_id
                    );
                }
            }
        }
        let fingerprint = root_verification_fingerprint(handle, task_id, &basis)?;
        let candidate_proof_ref = root_verification_candidate_proof(
            handle,
            task_id,
            revision,
            final_snapshot,
            prepared,
            run,
        )?;
        let record = handle
            .create_verification_record_with_evidence(
                task_id,
                Some(final_snapshot.to_string()),
                run.criteria.clone(),
                run.checks.clone(),
                Vec::new(),
                Vec::new(),
                None,
                status,
                handle.now_ms(),
                Some(fingerprint),
                Some(candidate_proof_ref),
            )
            .map_err(|e| ExecError::Internal(format!("root verification record write: {e}")))?;
        Ok((record, basis_digest))
    }

    /// Resolve the CURRENT instruction basis of the session's durable
    /// workspace for the proof basis (A fail-closed). The tree is read
    /// TWICE through the resolver's (root, epoch) cache: a changed epoch
    /// between the reads is an unstable tree, and EVERY store/resolver
    /// failure is typed — never collapsed into "no applicable instructions".
    fn resolve_instruction_basis(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Result<InstructionBasis, InstructionBasisError> {
        let row = handle.row().map_err(|e| match e.kind {
            faktor_core::ErrorKind::Malformed => InstructionBasisError::CorruptWorkspaceRelation {
                what: "session row".into(),
                detail: e.message,
            },
            _ => InstructionBasisError::StoreUnavailable {
                what: "session row".into(),
                detail: e.message,
            },
        })?;
        if row.workspace_id.raw() == 0 {
            return Err(InstructionBasisError::CorruptWorkspaceRelation {
                what: "session row".into(),
                detail: "workspace id 0 can never own an instruction tree".into(),
            });
        }
        let resolver = &self.agent.deps().instructions_resolver;
        let read = |pinned: Option<faktor_instructions::InstructionEpoch>| {
            resolver
                .resolve(row.workspace_id.raw(), pinned)
                .map_err(classify_rules_load_error)
        };
        match read(None)? {
            faktor_instructions::LoadedInstructions::Empty => {
                // The absence is only a valid basis while it is STABLE: a
                // second read that suddenly resolves a tree is a moving
                // world, not a no-instructions workspace.
                match read(None)? {
                    faktor_instructions::LoadedInstructions::Empty => {
                        Ok(InstructionBasis::NoApplicableInstructions)
                    }
                    faktor_instructions::LoadedInstructions::Loaded(_) => {
                        Err(InstructionBasisError::UnstableInstructionTree(
                            "the first read resolved no tree but the second resolved one".into(),
                        ))
                    }
                }
            }
            faktor_instructions::LoadedInstructions::Loaded(first) => {
                let faktor_instructions::LoadedInstructions::Loaded(second) = read(None)? else {
                    return Err(InstructionBasisError::UnstableInstructionTree(
                        "the first read resolved a tree but the second resolved none".into(),
                    ));
                };
                if first.epoch() != second.epoch() {
                    return Err(InstructionBasisError::UnstableInstructionTree(format!(
                        "the rule-tree epoch moved from {} to {} between the two reads of one basis",
                        first.epoch(),
                        second.epoch()
                    )));
                }
                Ok(InstructionBasis::Resolved {
                    epoch: first.epoch().as_u64(),
                })
            }
        }
    }

    /// The canonical proof basis of one orchestrated root verification: the
    /// exact task/revision/contract, candidate snapshot, integration sources,
    /// changed-file evidence, ordered check set, verifier version, PROBED
    /// tool versions (`rustc`/`cargo`/node/package managers + every check
    /// program + the custom verifier binary's digest), the verification
    /// environment projection (allowlisted fields, target triple, PATH-
    /// resolved executable identities), the resolved instruction epoch, the
    /// reviewer identity/model/payload digest and every immutable evidence
    /// digest contributing to a criterion PASS — the exact components a
    /// reusable record must match byte-for-byte. Built through the same
    /// bounded [`ProofBasis`] shape the session layer digests
    /// (`verification_record_reusable` compares the digests), and rebuilt
    /// immediately before every reuse consult so a tool/binary/environment
    /// change invalidates reuse in production, not only in synthetic tests.
    async fn root_verification_proof_basis(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        prepared: &PreparedRunIntegration,
        run: &faktor_agent::IntegratedRootVerification,
    ) -> Result<ProofBasis, ExecError> {
        let revision = handle
            .task_revision(task_id)
            .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
            .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
        let criteria: Vec<ProofBasisCriterion> = task
            .criteria()
            .into_iter()
            .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
            .map(|criterion| ProofBasisCriterion {
                criterion_id: criterion.id.to_string(),
                binding_digest: criterion.binding.as_ref().map(|b| b.content_digest()),
            })
            .collect();
        let checks: Vec<ProofBasisCheck> = run
            .checks
            .iter()
            .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
            .map(|check| ProofBasisCheck {
                check_id: check.check.clone(),
                program: check.program.clone(),
                args: check.args.clone(),
            })
            .collect();
        let changed_files_digest = if prepared.changed.is_empty() {
            String::new()
        } else {
            stable_list_digest(&prepared.changed)
        };
        // Probes through the daemon's ONE supervisor (bounded, deadline-
        // enforced). A missing supervisor degrades every probe to an
        // explicit marker; the lists are NEVER silently empty.
        let check_programs: Vec<String> = run
            .checks
            .iter()
            .map(|check| check.program.clone())
            .collect();
        let report = crate::proof_probe::probe_proof_basis(
            self.agent.deps().supervisor.as_ref(),
            &check_programs,
        )
        .await;
        // The CURRENT resolved instruction generation (the epoch IS the
        // resolved rule-tree digest). FIX (A): the basis is FAIL-CLOSED —
        // a store failure, a corrupt workspace relation, an unreadable rule
        // file, an unstable tree or a resolver error REFUSES the proof
        // (creation AND reuse); only a stable read that genuinely resolves
        // no tree yields the epoch-less NoApplicableInstructions basis.
        let instruction_basis = self.resolve_instruction_basis(handle).map_err(|e| {
            ExecError::Internal(format!(
                "root verification proof basis refused: instruction basis: {e}"
            ))
        })?;
        let reviewer_digest = reviewer_proof_basis_digest(run);
        let evidence_digests = criterion_pass_evidence_digests(prepared, run);
        Ok(ProofBasis {
            task_id: task_id.raw(),
            task_revision: revision.raw(),
            task_contract_digest: stable_list_digest(&task.acceptance_criteria),
            candidate_snapshot: prepared.candidate_snapshot.clone(),
            integration_sources_digest: prepared.sources_digest.clone(),
            changed_files_digest,
            checks,
            verification_impl_version: faktor_agent::runtime::VERIFICATION_IMPL_VERSION.to_string(),
            tool_versions: report.tools,
            env_projection: report.env_projection,
            instruction_epoch: instruction_basis.epoch(),
            criteria,
            reviewer_digest,
            evidence_digests,
        })
    }

    /// Drive the root task row across the machine's legal edges to
    /// `Verifying` (re-reading the revision before every edge). `Ok(false)`
    /// when the row cannot legally reach `Verifying` from its state
    /// (Blocked/terminal). Exact same edges the agent's gate driving uses.
    fn route_root_to_verifying(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
    ) -> Result<bool, ExecError> {
        for _ in 0..8 {
            let task = handle
                .get_task(task_id)
                .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
                .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
            let transition = match task.state {
                TaskState::Pending => TaskTransition::StartRunning,
                TaskState::Planning => TaskTransition::PlanComplete,
                TaskState::Running => TaskTransition::RequestVerification,
                TaskState::Waiting => TaskTransition::ResumeFromWaiting,
                TaskState::NeedsVerification => TaskTransition::StartVerification,
                TaskState::Verifying => return Ok(true),
                // Blocked is not silently unblocked here: the block is a
                // durable decision someone must resolve.
                TaskState::Blocked | TaskState::VerifiedComplete => return Ok(false),
                TaskState::Failed | TaskState::Cancelled => return Ok(false),
            };
            let revision = handle
                .task_revision(task_id)
                .map_err(|e| ExecError::Internal(format!("root task revision read: {e}")))?;
            handle
                .transition_task(task_id, revision, transition, None)
                .map_err(|e| {
                    ExecError::Conflict(format!(
                        "root task {task_id} could not be routed to Verifying: {e}"
                    ))
                })?;
        }
        Err(ExecError::Internal(format!(
            "root task {task_id} routing to Verifying exceeded its bounded edge count"
        )))
    }

    // ------------------------------------------------- shadow helpers (P0-48)

    /// The registered worktree root of the session (the durable owner root
    /// `owner_root_of` mirrors the orchestrated path: workspace/worktree
    /// rows only, never a guessed path).
    fn owner_root_of(
        &self,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
    ) -> Result<PathBuf, ExecError> {
        let row = handle.row()?;
        let wts = self
            .session
            .worktrees_of(row.workspace_id)?
            .into_iter()
            .filter(|w| (w.id as u64) == row.worktree_id.raw())
            .map(|w| PathBuf::from(w.path))
            .collect::<Vec<_>>();
        wts.first().cloned().ok_or_else(|| {
            ExecError::Conflict(format!(
                "session {parent} has no registered worktree row; shadowed mutating runs need a real owner worktree"
            ))
        })
    }

    /// Deterministic settlement of a durable shadow left by an interrupted
    /// drive BEFORE a new run begins:
    /// - shadow + `Failed`/`Cancelled` task row → discard;
    /// - shadow + `VerifiedComplete` task row → the owner landing must be
    ///   re-driven by the async settlement pipeline; a new run is refused
    ///   (typed Conflict) until the shadow retires;
    /// - shadow + non-terminal task row WITHOUT a shadow-world proof and no
    ///   live turn → the crashed drive's partial shadow is garbage → discard;
    /// - shadow + non-terminal task row WITH a shadow-world proof (the drive
    ///   certified the shadow but integration has not run) → the async
    ///   settlement owns the landing; a new run is refused (typed Conflict);
    /// - shadow + live turn → the previous drive is still running; a second
    ///   run cannot begin (typed Conflict).
    fn settle_existing_shadow(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
    ) -> Result<(), ExecError> {
        let shadows = self.shadows.as_ref().ok_or_else(|| {
            ExecError::Internal("shadow settlement requires the shadow service".into())
        })?;
        let Some(row) = shadows.active_shadow(parent)? else {
            return Ok(());
        };
        if !row.state.is_live() {
            return Ok(());
        }
        let task_id = handle.task_id()?;
        let state = handle.get_task(task_id)?.map(|t| t.state);
        match state {
            Some(TaskState::Failed) | Some(TaskState::Cancelled) => {
                shadows.discard(parent)?;
            }
            Some(TaskState::VerifiedComplete) => {
                return Err(ExecError::Conflict(format!(
                    "session {parent} carries the verified shadow {} whose owner integration must be re-settled before a new run starts",
                    row.shadow_id
                )));
            }
            Some(_) => {
                // Non-terminal task row: is the interrupted drive still
                // LIVE on the session? A durable ACTIVE turn record is the
                // precise marker (the crashed drive's record stays active;
                // an operator abort resolves it). With no live record the
                // crashed drive's partial shadow is garbage UNLESS the drive
                // certified the shadow world (a passing record at the
                // current revision): that proof marks a recoverable run
                // whose owner integration is still pending.
                let mid_turn = self
                    .session
                    .store()
                    .active_turn_record(parent)
                    .map(|r| r.is_some())
                    .map_err(|e| {
                        ExecError::from(classify_session_read(
                            "active turn record of the shadowed session",
                            faktor_core::Error::from(faktor_session::SessionError::from(e)),
                        ))
                    })?;
                if mid_turn {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} has a live shadow {} and an active drive; the interrupted run must be resumed or cancelled before a new shadowed task starts",
                        row.shadow_id
                    )));
                }
                // FIX 2: both reads feed a gate decision — a store failure is
                // an error, never "the shadow world was not proven".
                let revision = handle.task_revision(task_id).map_err(|e| {
                    ExecError::from(classify_session_read(
                        "task revision of the interrupted shadow run",
                        e.into(),
                    ))
                })?;
                let records = handle.list_verification_records(task_id).map_err(|e| {
                    ExecError::from(classify_session_read(
                        "verification records of the interrupted shadow run",
                        e.into(),
                    ))
                })?;
                let shadow_world_proven = records.iter().any(|record| {
                    record.status == VerificationStatus::Passed && record.revision == revision
                });
                if shadow_world_proven {
                    return Err(ExecError::Conflict(format!(
                        "session {parent} carries the certifiable shadow {} whose owner integration is still pending; re-settle the run before starting a new one",
                        row.shadow_id
                    )));
                }
                shadows.discard(parent)?;
            }
            None => {
                shadows.discard(parent)?;
            }
        }
        Ok(())
    }

    /// Post-drive hook of a shadowed run (spawned with the detached drive):
    /// settle the run through the ONE pipeline (a live shadow integrates
    /// prepare -> verify -> land -> steps -> complete). The runner only
    /// records step outcomes; completion goes through the durable gate.
    ///
    /// When the drive ended BEFORE the owner integration could run (the
    /// verifier may still certify in the background, or an integration
    /// conflict awaits the user's drift resolution), a BOUNDED watcher
    /// re-runs the decision on the next poll instead of leaving the shadow
    /// live forever.
    async fn after_shadowed_drive(self: &Arc<Self>, parent: SessionId) {
        let run_id = format!("tx-session-{}", parent.raw());
        match self
            .settle_run(RunSettlement::InSession { parent, run_id })
            .await
        {
            Ok(outcome) => {
                if matches!(
                    outcome.finalize.as_ref().map(|f| &f.action),
                    Some(ShadowFinalizeAction::Retained)
                        | Some(ShadowFinalizeAction::IntegrationBlocked)
                ) {
                    self.watch_shadow_settle(parent);
                }
            }
            Err(e) => eprintln!("shadowed-run settlement failed for session {parent}: {e}"),
        }
    }

    /// Bound of the post-drive shadow watcher: it may retry the durable
    /// finalize for at most this long while the session's shadow row is
    /// still LIVE (a non-terminal task row, or an IntegrationBlocked row
    /// waiting for the user to resolve drift). After it gives up, the
    /// deterministic settlement on the next run start (and every
    /// operator-facing [`Self::cancel_run`]) is the backstop — nothing is
    /// ever lost.
    const SHADOW_WATCH_DEADLINE: Duration = Duration::from_secs(120);
    /// Poll interval of the post-drive shadow watcher.
    const SHADOW_WATCH_INTERVAL: Duration = Duration::from_millis(250);

    /// Bounded re-arm of the post-drive settlement: a shadowed run whose
    /// drive ended with a LIVE shadow row (a non-terminal task row —
    /// verification in flight — or an IntegrationBlocked row awaiting the
    /// user's drift resolution) is re-settled on every poll until the row
    /// retires or the deadline passes. Late VerifiedComplete completions
    /// therefore integrate (and any then-runnable contract steps execute)
    /// and operator cancels discard the shadow without requiring a new run
    /// start. Every decision stays on the durable rows (settlement is
    /// per-state idempotent), so a crash of the watcher is recovered by the
    /// next run's deterministic settlement.
    fn watch_shadow_settle(self: &Arc<Self>, parent: SessionId) {
        let exec = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Self::SHADOW_WATCH_DEADLINE;
            loop {
                tokio::time::sleep(Self::SHADOW_WATCH_INTERVAL).await;
                if Instant::now() >= deadline {
                    return;
                }
                let run_id = format!("tx-session-{}", parent.raw());
                match exec
                    .settle_run(RunSettlement::InSession { parent, run_id })
                    .await
                {
                    Ok(outcome)
                        if !matches!(
                            outcome.finalize.as_ref().map(|f| &f.action),
                            Some(ShadowFinalizeAction::Retained)
                                | Some(ShadowFinalizeAction::IntegrationBlocked)
                        ) =>
                    {
                        // Integrated/discarded/no-live-shadow: nothing left
                        // to watch.
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("shadowed-run watch settlement failed for session {parent}: {e}");
                        return;
                    }
                }
            }
        });
    }

    /// Cancel ONE task run of the session, durably and exactly once:
    ///
    /// - an in-session run (linkage row) has its drive op ABORTED first
    ///   (queued prompts kill their queue row; live turns land the session
    ///   ReadyForNextTurn), then the session's task row is transitioned to
    ///   `Cancelled` (legal from every non-terminal state) and any shadow
    ///   of the run is discarded through the durable finalize;
    /// - an orchestrated run has every non-terminal child sent a durable
    ///   Cancel control through the runtime's exactly-once queue, and its
    ///   root task row (the cap/criteria row, when one exists) is
    ///   transitioned the same way.
    ///
    /// Typed refusals: unknown runs are `NotFound`, already-terminal runs
    /// are `Conflict` (a cancelled run is never cancelled twice), and a
    /// stale task-row revision (a concurrent verifier won the race) is a
    /// `Conflict` naming the run — retrying is the only forward path.
    pub fn cancel_run(self: &Arc<Self>, parent: SessionId, run_id: &str) -> Result<(), ExecError> {
        if run_id.is_empty()
            || run_id.len() > MAX_RUN_ID_CHARS
            || !run_id.is_ascii()
            || run_id.contains('/')
        {
            return Err(ExecError::NotFound(format!("task run {run_id:?}")));
        }
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let facts = parent_facts(&handle)?;
        let run_row = facts
            .iter()
            .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == run_id);
        if let Some((_, _, value)) = run_row {
            let row = TaskRunRow::decode(value)
                .map_err(|m| ExecError::Internal(format!("stored run row {run_id}: {m}")))?;
            return self.cancel_in_session_run(parent, &handle, &row);
        }
        let has_plan = facts
            .iter()
            .any(|(kind, key, _)| kind == PLAN_ROW_KIND && key == run_id);
        let children = OrchestratorRuntime::registry_rows(self.session.clone(), parent, run_id)?;
        if has_plan || !children.is_empty() {
            return self.cancel_orchestrated_run(&handle, run_id, &children);
        }
        Err(ExecError::NotFound(format!(
            "task run {run_id} under session {parent}"
        )))
    }

    /// Cancel one in-session run: abort its drive op, cancel the task row,
    /// discard any shadow (see [`Self::cancel_run`]).
    fn cancel_in_session_run(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
        row: &TaskRunRow,
    ) -> Result<(), ExecError> {
        let task_id = handle.task_id()?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        if task.as_ref().is_some_and(|t| t.state.is_terminal()) {
            return Err(ExecError::Conflict(format!(
                "task run {} is already terminal; a cancelled run is never cancelled twice",
                row.run_id
            )));
        }
        if let Some(op) = row.op_id {
            self.agent
                .abort_op(parent, Some(OpId::new(op)))
                .map_err(|e| ExecError::Internal(format!("abort of run op {op}: {}", e.message)))?;
        }
        if let Some(_task) = &task {
            let rev = handle
                .task_revision(task_id)
                .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
            handle
                .transition_task(task_id, rev, TaskTransition::Cancel, None)
                .map_err(|e| {
                    ExecError::Conflict(format!(
                        "task-row cancel of run {}: {e} (a concurrent verifier may have won; retry the cancel)",
                        row.run_id
                    ))
                })?;
        }
        // Cancelled is terminal: the durable finalize discards any shadow.
        if let Err(e) = self.finalize_shadow_run(parent) {
            eprintln!("shadow discard after cancel of run {}: {e}", row.run_id);
        }
        Ok(())
    }

    /// Cancel one orchestrated run: durable Cancel controls on every
    /// non-terminal child + the root task row (see [`Self::cancel_run`]).
    fn cancel_orchestrated_run(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        run_id: &str,
        children: &[super::ChildRuntime],
    ) -> Result<(), ExecError> {
        if !children
            .iter()
            .any(|c| !matches!(c.state, ChildState::Done | ChildState::Cancelled))
        {
            return Err(ExecError::Conflict(format!(
                "task run {run_id} has no non-terminal children; nothing to cancel"
            )));
        }
        for c in children {
            if matches!(c.state, ChildState::Done | ChildState::Cancelled) {
                continue;
            }
            self.orchestrator
                .control_child(&c.child_id, faktor_session::child::ChildControl::Cancel)
                .map_err(|e| match e {
                    ExecError::NotFound(m) => ExecError::Internal(format!(
                        "child {} of the cancelled run {run_id} is unknown to the runtime: {m}",
                        c.child_id
                    )),
                    ExecError::Conflict(m) => {
                        ExecError::Conflict(format!("child cancel of {}: {m}", c.child_id))
                    }
                    other => ExecError::Internal(format!(
                        "child cancel of {} failed: {other}",
                        c.child_id
                    )),
                })?;
        }
        let task_id = handle.task_id()?;
        let task = handle
            .get_task(task_id)
            .map_err(|e| ExecError::Internal(format!("task row read: {e}")))?;
        if let Some(t) = task {
            if !t.state.is_terminal() {
                let rev = handle
                    .task_revision(task_id)
                    .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
                handle
                    .transition_task(task_id, rev, TaskTransition::Cancel, None)
                    .map_err(|e| {
                        ExecError::Conflict(format!(
                            "root task-row cancel of run {run_id}: {e} (a concurrent verifier may have won; retry the cancel)"
                        ))
                    })?;
            }
        }
        Ok(())
    }

    /// The durable lifecycle decision of a shadowed run (P0-48/49): read the
    /// session's shadow row + task row and act ONCE per terminal state:
    ///
    /// - `Failed`/`Cancelled` → discard the shadow;
    /// - `VerifiedComplete` → RETAINED: the owner landing is owned by the
    ///   async settlement pipeline ([`Self::settle_shadowed_in_session`]);
    ///   a completed task can only have reached that state through a
    ///   successful landing, so this path is the crash window between the
    ///   completion gate and the shadow retirement, recovered by the next
    ///   settlement;
    /// - any non-terminal state → retained (the drive may continue or the
    ///   settlement may still integrate; see [`Self::watch_shadow_settle`]
    ///   and every next run's deterministic settlement).
    ///
    /// `Ok(None)` when no live shadow exists (plain runs). This function
    /// NEVER lands content: the single commitment engine is the
    /// transactional [`Self::land_verified_integration`] pipeline.
    pub fn finalize_shadow_run(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<Option<ShadowFinalize>, ExecError> {
        let Some(shadows) = self.shadows() else {
            return Ok(None);
        };
        let Some(row) = shadows.active_shadow(parent)? else {
            return Ok(None);
        };
        if !row.state.is_live() {
            return Ok(None);
        }
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let task_id = handle.task_id()?;
        let Some(task) = handle.get_task(task_id)? else {
            return Ok(None);
        };
        match task.state {
            TaskState::Failed | TaskState::Cancelled => {
                shadows
                    .discard(parent)
                    .map_err(|e| ExecError::from_shadow("shadow discard", e))?;
                Ok(Some(ShadowFinalize {
                    action: ShadowFinalizeAction::Discarded,
                    merged: Vec::new(),
                    rejected: Vec::new(),
                    conflicts: Vec::new(),
                }))
            }
            _ => Ok(Some(retained_shadow_finalize())),
        }
    }

    // ------------------------------------------------------ tournament entry

    /// Start a multi-candidate implementation tournament (N = 2..=4): the
    /// SAME goal and the SAME criteria are fanned out to N ISOLATED
    /// candidate worktrees through the existing executor/assignment
    /// machinery (an N-item plan of `Implementation` items under
    /// `OwnershipSpec::IsolatedWorktree`), and the durable
    /// `TournamentStarted` ledger anchor is written on the parent session.
    /// The candidate child ids are the deterministic plan-order ids
    /// (`child-0..child-{n-1}`) the assignment compile mints; the receipt
    /// returns them plus the orchestrated run id whose registry rows carry
    /// the candidate drives.
    pub fn start_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        goal: &str,
        criteria: &[String],
        n: usize,
        mutation_mode: Option<MutationMode>,
    ) -> Result<TournamentReceipt, ExecError> {
        self.start_tournament_with(
            parent,
            TournamentStartRequest {
                goal: goal.to_string(),
                criteria: criteria.to_vec(),
                n,
                mutation_mode,
                ..Default::default()
            },
        )
    }

    /// [`Self::start_tournament`] with the full request (model, budgets,
    /// ceilings, files, isolated root, crash seam). The tournament engine
    /// enforces the candidate band and the criterion bounds; the task
    /// start validates the plan through the ONE task-start authority.
    pub fn start_tournament_with(
        self: &Arc<Self>,
        parent: SessionId,
        req: TournamentStartRequest,
    ) -> Result<TournamentReceipt, ExecError> {
        req.validate()?;
        let criteria = build_tournament_criteria(&req.criteria)?;
        let tournament_id = format!("tour-{:016x}", self.session.next_op_id().raw());
        let mut tournament = crate::tournament::Tournament::new(
            &tournament_id,
            "pending-run",
            &req.goal,
            criteria,
            req.n,
        )
        .map_err(tournament_exec_error)?;
        // All candidates receive the byte-identical goal summary and the
        // byte-identical criterion ids/specs: the assertion is a typed
        // refusal BEFORE any child spawns (a drift never fans out).
        let canonical = crate::tournament::canonical_criteria_text(&tournament.criteria);
        let criterion_specs: Vec<String> =
            tournament.criteria.iter().map(|c| c.spec.clone()).collect();
        let work_items: Vec<WorkItem> = (0..req.n)
            .map(|i| {
                let mut item = WorkItem::new(
                    format!("candidate-{i}"),
                    canonical.clone(),
                    WorkKind::Implementation,
                );
                item.acceptance_checks = criterion_specs.clone();
                item
            })
            .collect();
        crate::tournament::assert_candidates_identical(&work_items, &tournament.criteria)
            .map_err(tournament_exec_error)?;
        let run_request = TaskRunRequest {
            goal: req.goal.clone(),
            work_items,
            model: req.model,
            max_tokens: req.max_tokens,
            max_cost_micro: req.max_cost_micro,
            criteria: req.criteria.clone(),
            mutation_mode: req.mutation_mode,
            files: req.files,
            parent_caps: req.parent_caps,
            ceilings: req.ceilings,
            isolated_root: req.isolated_root,
            crash_seam: req.crash_seam,
            ..Default::default()
        };
        let receipt = self.start_task(parent, run_request)?;
        if receipt.mode != TaskRunMode::Orchestrated {
            return Err(ExecError::Internal(
                "a tournament start must dispatch to the orchestrated child path".into(),
            ));
        }
        tournament.run_family = receipt.run_id.clone();
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        tournament
            .persist_started(&handle)
            .map_err(tournament_exec_error)?;
        Ok(TournamentReceipt {
            tournament_id,
            run_id: receipt.run_id,
            candidates: tournament
                .candidates
                .iter()
                .map(|c| c.child_id.clone())
                .collect(),
        })
    }

    /// Read ONE tournament's durable state (reconstructed from the ledger)
    /// with RUNNING candidates refreshed from the run's registry rows
    /// (location, base revision, observed child state). Unknown ids are
    /// typed `NotFound`.
    pub fn tournament_state(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        Ok(tournament)
    }

    /// Every reconstructed tournament of one session, oldest first.
    pub fn tournaments_of(
        self: &Arc<Self>,
        parent: SessionId,
    ) -> Result<Vec<crate::tournament::Tournament>, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        crate::tournament::Tournament::reopen(&handle).map_err(tournament_exec_error)
    }

    /// Build the settlement skeleton of ONE candidate from its durable
    /// state: the DERIVED check set of the tournament, the candidate's
    /// worktree/base revision from the run's registry row, and its observed
    /// terminal state. The caller attaches the verification record, the
    /// independent review and the measured cost/wall axes. A candidate that
    /// has not settled (`Running`) is a typed Conflict.
    pub fn candidate_settlement(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        child_id: &str,
        reason: &str,
    ) -> Result<crate::tournament::CandidateSettlement, ExecError> {
        let tournament = self.tournament_state(parent, tournament_id)?;
        let candidate = tournament
            .candidates
            .iter()
            .find(|c| c.child_id == child_id)
            .ok_or_else(|| {
                ExecError::NotFound(format!(
                    "candidate {child_id} of tournament {tournament_id}"
                ))
            })?;
        if matches!(
            candidate.state,
            crate::tournament::CandidateState::Running
                | crate::tournament::CandidateState::Discarded
        ) {
            return Err(ExecError::Conflict(format!(
                "candidate {child_id} of tournament {tournament_id} has not settled (state {:?})",
                candidate.state
            )));
        }
        Ok(crate::tournament::CandidateSettlement {
            child_id: candidate.child_id.clone(),
            worktree: candidate.worktree.clone(),
            base_revision: candidate.base_revision.clone(),
            state: candidate.state,
            verification: None,
            verification_pass: None,
            checks: tournament.check_specs(),
            review: None,
            cost_micro: 0,
            wall_ms: 0,
            reason: reason.to_string(),
        })
    }

    /// Settle ONE candidate: validate the settlement against the durable
    /// tournament (byte-identical derived check set, independent reviewer),
    /// then persist the `CandidateSettled` ledger row.
    pub fn settle_tournament_candidate(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        settlement: crate::tournament::CandidateSettlement,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        tournament
            .settle_candidate(settlement.clone())
            .map_err(tournament_exec_error)?;
        tournament
            .persist_settlement(&handle, &settlement)
            .map_err(tournament_exec_error)?;
        Ok(tournament)
    }

    /// Decide ONE tournament with the documented deterministic ordering
    /// (verification pass > review rank > cost > ordinal): persist the
    /// `TournamentDecided` audit row FIRST, then discard every loser
    /// (registry row settled terminal, isolated worktree directory + row
    /// removed). The winner's worktree is only PROPOSED — integration stays
    /// the explicit approved-merge path and is never called here.
    pub fn decide_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
    ) -> Result<crate::tournament::TournamentDecision, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        let decision = tournament.decide().map_err(tournament_exec_error)?;
        tournament
            .persist_decision(
                &handle,
                faktor_session::TOURNAMENT_OUTCOME_DECIDED,
                &decision.rationale,
            )
            .map_err(tournament_exec_error)?;
        for (loser_id, _why) in &decision.discarded {
            if let Some(candidate) = tournament
                .candidates
                .iter()
                .find(|c| &c.child_id == loser_id)
            {
                crate::tournament::discard_candidate_worktree(
                    &self.session,
                    &handle,
                    &tournament.run_family,
                    candidate,
                )
                .map_err(tournament_exec_error)?;
            }
        }
        Ok(decision)
    }

    /// Abort ONE tournament: every candidate is discarded (registry rows
    /// settled terminal; isolated worktrees removed), the terminal
    /// `TournamentDecided { outcome: aborted }` audit row records WHY, and
    /// no winner is proposed.
    pub fn abort_tournament(
        self: &Arc<Self>,
        parent: SessionId,
        tournament_id: &str,
        reason: &str,
    ) -> Result<crate::tournament::Tournament, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let mut tournament = crate::tournament::Tournament::load(&handle, tournament_id)
            .map_err(tournament_exec_error)?;
        crate::tournament::refresh_candidates_from_registry(&self.session, parent, &mut tournament)
            .map_err(tournament_exec_error)?;
        tournament.abort(reason).map_err(tournament_exec_error)?;
        let rationale = if reason.trim().is_empty() {
            "aborted by operator; every candidate discarded".to_string()
        } else {
            reason.to_string()
        };
        tournament
            .persist_decision(
                &handle,
                faktor_session::TOURNAMENT_OUTCOME_ABORTED,
                &rationale,
            )
            .map_err(tournament_exec_error)?;
        for candidate in &tournament.candidates {
            crate::tournament::discard_candidate_worktree(
                &self.session,
                &handle,
                &tournament.run_family,
                candidate,
            )
            .map_err(tournament_exec_error)?;
        }
        Ok(tournament)
    }
}

/// One tournament start: goal + criteria + candidate count + the
/// per-run knobs the one task-start authority understands. The strict
/// server DTO maps onto this; the programmatic/test boundary may set the
/// isolated root and crash seam directly.
#[derive(Debug, Clone)]
pub struct TournamentStartRequest {
    pub goal: String,
    pub criteria: Vec<String>,
    pub n: usize,
    /// Decoded for wire compatibility only: the sole decodable value is
    /// [`MutationMode::Shadow`] (a `direct_compat` value is a strict decode
    /// error naming the removal). Candidates are ALWAYS isolated
    /// worktrees — no value can change that.
    pub mutation_mode: Option<MutationMode>,
    pub model: Option<String>,
    pub max_tokens: Option<u64>,
    pub max_cost_micro: Option<u64>,
    /// Files attached to every candidate's ordinary drive.
    pub files: Vec<String>,
    /// Capability ceiling of the parent. Default: read+write on the whole
    /// workspace (candidate writes land ONLY in daemon-allocated isolated
    /// worktrees; integration stays explicit).
    pub parent_caps: CapabilitySet,
    pub ceilings: super::Ceilings,
    /// Root under which isolated candidate workspaces are created. Empty =
    /// the executor's daemon-owned [`CandidateWorkspaceService`] allocates
    /// one (the wire never carries a path).
    pub isolated_root: PathBuf,
    /// Deterministic crash seam (adversarial tests only).
    pub crash_seam: Option<CrashSeam>,
}

impl Default for TournamentStartRequest {
    fn default() -> Self {
        Self {
            goal: String::new(),
            criteria: Vec::new(),
            n: 0,
            mutation_mode: None,
            model: None,
            max_tokens: None,
            max_cost_micro: None,
            files: Vec::new(),
            parent_caps: child_caps(WorkKind::Implementation),
            ceilings: super::Ceilings::default(),
            isolated_root: PathBuf::new(),
            crash_seam: None,
        }
    }
}

impl TournamentStartRequest {
    /// Structural validation (the engine enforces the typed candidate band
    /// and criterion bounds; this only checks the request shape before any
    /// durable write).
    pub fn validate(&self) -> Result<(), ExecError> {
        if !(crate::tournament::MIN_CANDIDATES..=crate::tournament::MAX_CANDIDATES)
            .contains(&self.n)
        {
            return Err(ExecError::InvalidPlan(format!(
                "tournament candidate count {} is outside the supported band {}..={}",
                self.n,
                crate::tournament::MIN_CANDIDATES,
                crate::tournament::MAX_CANDIDATES
            )));
        }
        if self.goal.trim().is_empty() {
            return Err(ExecError::InvalidPlan("tournament goal is empty".into()));
        }
        if self.criteria.is_empty() {
            return Err(ExecError::InvalidPlan(
                "a tournament needs at least one criterion".into(),
            ));
        }
        self.ceilings.validate().map_err(ExecError::InvalidPlan)?;
        Ok(())
    }
}

/// The receipt of one accepted tournament start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TournamentReceipt {
    pub tournament_id: String,
    pub run_id: String,
    /// The deterministic candidate child ids (`child-0..child-{n-1}`).
    pub candidates: Vec<String>,
}

fn build_tournament_criteria(
    specs: &[String],
) -> Result<Vec<crate::tournament::Criterion>, ExecError> {
    let mut criteria = Vec::with_capacity(specs.len());
    for spec in specs {
        criteria.push(crate::tournament::Criterion::derive(spec).map_err(tournament_exec_error)?);
    }
    Ok(criteria)
}

/// Map the tournament engine's typed refusals onto the executor error
/// space (the HTTP layer maps these onto 400/404/409 exactly like every
/// other executor refusal).
fn tournament_exec_error(e: crate::tournament::TournamentError) -> ExecError {
    use crate::tournament::TournamentError as T;
    match &e {
        T::NotFound(_) => ExecError::NotFound(e.to_string()),
        T::Oversized(_) => ExecError::Oversized(e.to_string()),
        T::NotOpen(_) | T::DuplicateSettlement(_) | T::NoEligibleWinner(_) => {
            ExecError::Conflict(e.to_string())
        }
        T::Corrupt { .. } | T::Ledger(_) | T::Cleanup(_) => ExecError::Internal(e.to_string()),
        T::InvalidCandidateCount { .. }
        | T::InvalidCriteriaCount { .. }
        | T::InvalidCriterion(_)
        | T::DuplicateCriterion(_)
        | T::InvalidId(_)
        | T::UnknownCandidate(_)
        | T::IllegalSettlementState(_)
        | T::VerificationSpecDrift
        | T::ReviewNotIndependent(_) => ExecError::InvalidPlan(e.to_string()),
    }
}

/// The default effective capability grant of one work item's child:
/// read-only items read the workspace; mutating items read + write it
/// (the permission requester still gates every actual tool call — these
/// sets are the orchestrator's typed policy record).
pub fn child_caps(kind: WorkKind) -> CapabilitySet {
    let mut grants = vec![CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
    )];
    if kind.is_mutating() {
        grants.push(CapabilityGrant::new(
            LatticeCap::WriteWorkspace,
            ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
        ));
    }
    CapabilitySet::from_grants(grants).expect("wildcard grants are sane")
}

/// The read-only file capability of an item whose writes are NOT
/// file-level: read-only items (all of them) and semantic-entity items
/// (their writes are provider-scoped; file-level WriteWorkspace would
/// exceed the ownership their compile assigned).
fn read_child_caps() -> CapabilitySet {
    CapabilitySet::from_grants([CapabilityGrant::new(
        LatticeCap::ReadWorkspace,
        ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
    )])
    .expect("wildcard grants are sane")
}

/// Write one linkage row under the session (bounded value; loud refusal
/// when the row would exceed the memory-fact cap).
fn put_run_row(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    row: &TaskRunRow,
) -> Result<(), ExecError> {
    if run_id.is_empty()
        || run_id.len() > MAX_RUN_ID_CHARS
        || !run_id.is_ascii()
        || run_id.contains('/')
    {
        return Err(ExecError::Oversized(format!(
            "run id must be 1..={MAX_RUN_ID_CHARS} ASCII characters without '/'"
        )));
    }
    let value = serde_json::to_string(row)
        .map_err(|e| ExecError::Internal(format!("run row serialization: {e}")))?;
    if value.len() > MAX_TASK_RUN_ROW_BYTES {
        return Err(ExecError::Oversized(format!(
            "task run row of {} bytes exceeds the {MAX_TASK_RUN_ROW_BYTES}-byte bound",
            value.len()
        )));
    }
    handle
        .upsert_memory_fact(TASK_RUN_ROW_KIND, run_id, &value)
        .map_err(|e| ExecError::Internal(format!("task run row write: {}", e.message)))?;
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Record one run's accepted completion contract before its first model
/// call (P2 record-first): the durable `CompletionContractSet` row lands at
/// the task row's CURRENT revision and is immutable per revision. `None`
/// and the explicit all-false contract write nothing — the default
/// completion path stays byte-identical.
fn record_completion_contract(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    contract: Option<CompletionContract>,
) -> Result<(), ExecError> {
    let Some(contract) = contract.filter(|c| !c.is_default()) else {
        return Ok(());
    };
    let revision = handle
        .task_revision(task_id)
        .map_err(|e| ExecError::Internal(format!("task revision read: {e}")))?;
    handle
        .set_completion_contract(task_id, revision, contract)
        .map(|_seq| ())
        .map_err(|e| match e {
            faktor_session::TaskError::CompletionContractImmutable { .. }
            | faktor_session::TaskError::RevisionMismatch { .. } => {
                ExecError::Conflict(format!("completion contract: {e}"))
            }
            other => ExecError::Internal(format!("completion contract seed: {other}")),
        })
}

/// TRUE when the durable verification fact of the session's latest genuine
/// end says the deterministic verification PASSED. UI/reporting ONLY (the
/// `SettlementOutcome.verified` projection): it is NEVER an authorization
/// for a completion side effect — those run exclusively through
/// [`CompletionStepRunner::run_completion_steps`] with an immutable
/// verification-record proof.
fn verification_passed(handle: &faktor_session::SessionHandle) -> bool {
    let Ok(facts) = handle.memory_facts() else {
        return false;
    };
    let Some((_, _, last)) = facts
        .iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(last) else {
        return false;
    };
    value.get("status").and_then(|s| s.as_str()) == Some("passed")
}

/// The ONE composed root verification verdict of one orchestrated run: the
/// authoritative status of `verify_prepared_integration` and the only
/// authorization a [`VerifiedRunIntegration`] may carry.
///
/// `Passed` ONLY when EVERY required check passed AND every required
/// criterion passed. A failed required check/criterion is `Failed`; a
/// required check/criterion that produced no verdict — explicitly
/// [`CriterionBinding::Unavailable`], a required-check binding that resolves
/// to nothing, or a non-pass with no evidence — is `Unavailable`, never
/// `Pending`. Advisory (`Preferred`) criteria, optional checks and their
/// verdicts never block.
pub fn compose_root_verification_status(
    checks: &[CheckExecution],
    criteria: &[CriterionVerification],
) -> VerificationStatus {
    let check_verdicts = checks
        .iter()
        .filter(|check| check.required)
        .map(required_check_verdict);
    let criterion_verdicts = criteria
        .iter()
        .filter(|criterion| criterion_is_required(criterion))
        .map(|criterion| criterion_verification_status(checks, criterion));
    merge_verdict_sides(check_verdicts.chain(criterion_verdicts))
}

/// The STRICTER no-op rule of an EMPTY aggregate change set: there is no
/// check evidence to lean on, so EVERY criterion verdict — advisory included
/// — must pass before the empty run may compose `Passed`. A non-passing
/// advisory criterion degrades this verdict to `Failed`/`Unavailable`; it
/// never silently passes.
pub fn compose_no_op_root_verification_status(
    checks: &[CheckExecution],
    criteria: &[CriterionVerification],
) -> VerificationStatus {
    let check_verdicts = checks
        .iter()
        .filter(|check| check.required)
        .map(required_check_verdict);
    merge_verdict_sides(
        check_verdicts.chain(
            criteria
                .iter()
                .map(|criterion| criterion_verification_status(checks, criterion)),
        ),
    )
}

/// The verdict of ONE required check row: an unknown/pending status is
/// missing evidence (`Unavailable`) — never a pass and never a `Pending`.
fn required_check_verdict(check: &CheckExecution) -> VerificationStatus {
    match check.status {
        VerificationStatus::Passed => VerificationStatus::Passed,
        VerificationStatus::Failed => VerificationStatus::Failed,
        _ => VerificationStatus::Unavailable,
    }
}

/// Whether one criterion verdict belongs to a REQUIRED criterion. The key
/// is the persisted task entry: typed V2 entries decode to their
/// requirement; every other entry migrates as Required
/// ([`faktor_session::task::Criterion::legacy`]), so plain legacy text can
/// never be silently demoted to advisory.
fn criterion_is_required(criterion: &CriterionVerification) -> bool {
    match faktor_session::task::Criterion::decode(&criterion.criterion_key) {
        Some(typed) => typed.requirement.is_required(),
        None => true,
    }
}

/// The typed verdict of ONE criterion verification. A pass is a pass; a
/// `RequiredCheck` binding resolves against the EXECUTED check rows exactly
/// like the evaluator does (id + command digest) — a check that ran and
/// failed is `Failed`, an unresolved/ambiguous/missing binding is
/// `Unavailable`. The explicit honest-unknown binding and a non-pass with no
/// evidence are `Unavailable` (missing proof, never a failure conclusion and
/// never `Pending`); every other non-pass with evidence is `Failed`.
fn criterion_verification_status(
    checks: &[CheckExecution],
    criterion: &CriterionVerification,
) -> VerificationStatus {
    if criterion.passed {
        return VerificationStatus::Passed;
    }
    match &criterion.binding {
        Some(CriterionBinding::Unavailable { .. }) => return VerificationStatus::Unavailable,
        Some(CriterionBinding::RequiredCheck {
            check_id,
            command_digest,
        }) => return required_check_binding_verdict(checks, check_id, command_digest),
        _ => {}
    }
    if criterion
        .evidence
        .as_deref()
        .is_none_or(|evidence| evidence.trim().is_empty())
    {
        return VerificationStatus::Unavailable;
    }
    VerificationStatus::Failed
}

/// Resolve ONE required-check binding against the executed check rows using
/// the evaluator's exact identity (check id when present, canonical command
/// digest always). Exactly one match is required: a failed resolution is
/// `Failed`; anything unresolved or ambiguous is missing evidence
/// (`Unavailable`) — never a pass.
fn required_check_binding_verdict(
    checks: &[CheckExecution],
    check_id: &str,
    command_digest: &str,
) -> VerificationStatus {
    let canonical = |check: &CheckExecution| {
        if check.args.is_empty() {
            check.program.clone()
        } else {
            format!("{} {}", check.program, check.args.join(" "))
        }
    };
    let matches: Vec<&CheckExecution> = checks
        .iter()
        .filter(|check| {
            (check_id.is_empty() || check.check == check_id)
                && command_binding_digest(&canonical(check)) == command_digest
        })
        .collect();
    match matches.as_slice() {
        [only] => match only.status {
            VerificationStatus::Failed => VerificationStatus::Failed,
            VerificationStatus::Passed => {
                // The criterion verdict itself says not-passed; a passing row
                // cannot re-authorize it, so the contradiction blocks.
                VerificationStatus::Failed
            }
            _ => VerificationStatus::Unavailable,
        },
        _ => VerificationStatus::Unavailable,
    }
}

/// Fold verdict sides into ONE status with failure dominance: any failure
/// wins, otherwise any unavailable/missing side makes the whole
/// `Unavailable`; only all-passing sides compose `Passed`.
fn merge_verdict_sides(verdicts: impl Iterator<Item = VerificationStatus>) -> VerificationStatus {
    let mut unavailable = false;
    for verdict in verdicts {
        match verdict {
            VerificationStatus::Passed => {}
            VerificationStatus::Failed => return VerificationStatus::Failed,
            _ => unavailable = true,
        }
    }
    if unavailable {
        VerificationStatus::Unavailable
    } else {
        VerificationStatus::Passed
    }
}

/// The strictest of two verdicts on the same evidence (failure beats
/// unavailable beats pass): the agent's check-side acceptance never softens
/// the composed criterion verdict and vice versa.
fn merge_verification_status(
    left: VerificationStatus,
    right: VerificationStatus,
) -> VerificationStatus {
    let rank = |status: VerificationStatus| match status {
        VerificationStatus::Passed => 0u8,
        VerificationStatus::Unavailable
        | VerificationStatus::Pending
        | VerificationStatus::Running => 1,
        VerificationStatus::Failed => 2,
    };
    match rank(left).max(rank(right)) {
        0 => VerificationStatus::Passed,
        1 => VerificationStatus::Unavailable,
        _ => VerificationStatus::Failed,
    }
}

/// The durable fact tag of one composed verdict. `Pending`/`Running` never
/// compose, but map honestly to the legacy `pending` fact tag.
fn verification_status_tag(status: VerificationStatus) -> &'static str {
    match status {
        VerificationStatus::Passed => "passed",
        VerificationStatus::Failed => "failed",
        VerificationStatus::Unavailable => "unavailable",
        VerificationStatus::Pending | VerificationStatus::Running => "pending",
    }
}

/// The domain separator of the canonical proof-basis digest (version 3):
/// the digest can never collide with an incidental serde/JCS encoding of the
/// same value, and the version is folded in so a future encoding change
/// invalidates every older digest instead of silently reusing it.
pub const PROOF_BASIS_DIGEST_DOMAIN: &[u8] = b"FAKTOR_PROOF_BASIS\0";
/// The canonical proof-basis encoding version.
pub const PROOF_BASIS_DIGEST_VERSION: u64 = 3;

/// The canonical, TOTAL proof-basis payload: `FAKTOR_PROOF_BASIS\0` + the
/// version + every basis field in a fixed order with length-prefixed legs.
/// Total by construction (no serde, no `Result`, no `unwrap_or_default`):
/// every `String`/`u64`/`Vec` has exactly one encoding, so a serialization
/// failure is impossible and the digest is stable under value-level equality.
pub fn canonical_proof_basis_payload(basis: &ProofBasis) -> Vec<u8> {
    fn put_u64(out: &mut Vec<u8>, value: u64) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn put_str(out: &mut Vec<u8>, value: &str) {
        put_u64(out, value.len() as u64);
        out.extend_from_slice(value.as_bytes());
    }
    fn put_opt_str(out: &mut Vec<u8>, value: Option<&str>) {
        match value {
            Some(value) => {
                out.push(1);
                put_str(out, value);
            }
            None => out.push(0),
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(PROOF_BASIS_DIGEST_DOMAIN);
    put_u64(&mut out, PROOF_BASIS_DIGEST_VERSION);
    put_u64(&mut out, basis.task_id);
    put_u64(&mut out, basis.task_revision);
    put_str(&mut out, &basis.task_contract_digest);
    put_str(&mut out, &basis.candidate_snapshot);
    put_str(&mut out, &basis.integration_sources_digest);
    put_str(&mut out, &basis.changed_files_digest);
    put_u64(&mut out, basis.checks.len() as u64);
    for check in &basis.checks {
        put_str(&mut out, &check.check_id);
        put_str(&mut out, &check.program);
        put_u64(&mut out, check.args.len() as u64);
        for arg in &check.args {
            put_str(&mut out, arg);
        }
    }
    put_str(&mut out, &basis.verification_impl_version);
    put_u64(&mut out, basis.tool_versions.len() as u64);
    for tool in &basis.tool_versions {
        put_str(&mut out, &tool.tool);
        put_str(&mut out, &tool.version);
    }
    put_u64(&mut out, basis.env_projection.len() as u64);
    for (key, value) in &basis.env_projection {
        put_str(&mut out, key);
        put_str(&mut out, value);
    }
    match basis.instruction_epoch {
        Some(epoch) => {
            out.push(1);
            put_u64(&mut out, epoch);
        }
        None => out.push(0),
    }
    put_u64(&mut out, basis.criteria.len() as u64);
    for criterion in &basis.criteria {
        put_str(&mut out, &criterion.criterion_id);
        put_opt_str(&mut out, criterion.binding_digest.as_deref());
    }
    put_opt_str(&mut out, basis.reviewer_digest.as_deref());
    put_u64(&mut out, basis.evidence_digests.len() as u64);
    for digest in &basis.evidence_digests {
        put_str(&mut out, digest);
    }
    out
}

/// The canonical domain/version-separated proof-basis digest — the ONLY
/// digest the orchestrator writes into a record fingerprint or compares for
/// reuse. A fingerprint carrying the retired serde-bytes digest (or no
/// digest at all) is never reusable.
pub fn canonical_proof_basis_digest(basis: &ProofBasis) -> String {
    let payload = canonical_proof_basis_payload(basis);
    format!("blake3:{}", blake3::hash(&payload).to_hex())
}

/// The canonical-basis reuse consult: a candidate record is reusable ONLY
/// when its persisted proof-basis digest equals the canonical digest of the
/// current basis. Refusals carry the recorded vs current digests, and an
/// absent/legacy digest is an honest unknown — never a license.
fn canonical_proof_reuse(
    handle: &faktor_session::SessionHandle,
    record_id: VerificationRecordId,
    basis_digest: &str,
) -> Result<ProofReuse, ExecError> {
    let Some(record) = handle
        .get_verification_record(record_id)
        .map_err(|e| ExecError::Internal(format!("proof reuse record read: {e}")))?
    else {
        return Ok(ProofReuse::Refused {
            reason: format!("record {record_id} does not exist"),
        });
    };
    let Some(recorded) = record
        .environment_fingerprint
        .as_ref()
        .and_then(|f| f.proof_basis_digest.as_deref())
    else {
        return Ok(ProofReuse::Refused {
            reason: format!("record {record_id} carries no proof basis; it is never reusable"),
        });
    };
    if recorded == basis_digest {
        Ok(ProofReuse::Allowed)
    } else {
        Ok(ProofReuse::Refused {
            reason: format!(
                "record {record_id} is bound to proof basis {recorded}, but the current \
                 canonical basis is {basis_digest}; reuse requires an identical basis"
            ),
        })
    }
}

/// The bounded v20 environment fingerprint of one orchestrated ROOT record:
/// the proof-basis digest (the reuse key), the task-contract digest and the
/// check-basis digest, all deterministic for identical inputs. Empty tool/
/// manifest/env projections are HONEST absences at the executor layer (the
/// agent's attempt records carry their own observed fingerprint).
fn root_verification_fingerprint(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    basis: &ProofBasis,
) -> Result<EnvironmentFingerprint, ExecError> {
    let task = handle
        .get_task(task_id)
        .map_err(|e| ExecError::Internal(format!("root task row read: {e}")))?
        .ok_or_else(|| ExecError::Internal(format!("root task {task_id} missing")))?;
    let check_basis: Vec<String> = basis
        .checks
        .iter()
        .map(|c| format!("{}|{}|{}", c.check_id, c.program, c.args.join(" ")))
        .collect();
    Ok(EnvironmentFingerprint {
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        toolchain_versions: Vec::new(),
        manifest_hashes: Vec::new(),
        lockfile_hashes: Vec::new(),
        instruction_epoch: None,
        base_tree_hash: None,
        task_contract_hash: stable_list_digest(&task.acceptance_criteria),
        check_argv_cwd_env_hash: stable_list_digest(&check_basis),
        verification_impl_version: faktor_agent::runtime::VERIFICATION_IMPL_VERSION.to_string(),
        proof_basis_digest: Some(canonical_proof_basis_digest(basis)),
    })
}

/// The compact candidate-proof reference of one orchestrated ROOT record:
/// the immutable run base, the verified candidate snapshot, the integration
/// sources digest and the changed-file digest, so both IDEs can show what
/// the proof was based on and completion can re-derive it.
fn root_verification_candidate_proof(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
    revision: TaskRevision,
    final_snapshot: &str,
    prepared: &PreparedRunIntegration,
    run: &faktor_agent::IntegratedRootVerification,
) -> Result<CandidateProofRef, ExecError> {
    let accounting = handle
        .accounting_snapshot_digest(task_id)
        .map_err(|e| ExecError::Internal(format!("accounting snapshot digest: {e}")))?;
    Ok(CandidateProofRef {
        task_revision: revision,
        base_manifest_hash: stable_list_digest(&[]),
        candidate_manifest_hash: stable_list_digest(&[final_snapshot.to_string()]),
        source_diff_evidence: None,
        risk_report_evidence: None,
        accounting_snapshot_digest: accounting,
        run_id: Some(prepared.run_id.clone()),
        run_base_snapshot: Some(prepared.base_snapshot.clone()),
        candidate_snapshot: Some(final_snapshot.to_string()),
        sources_digest: (!prepared.sources_digest.is_empty())
            .then(|| prepared.sources_digest.clone()),
        changed_files_digest: (!run.changed.is_empty()).then(|| stable_list_digest(&run.changed)),
    })
}

/// Write the REAL root verification fact of an orchestrated run (the SAME
/// `verification`/`last` shape the agent's genuine ends write): the executed
/// checks with their real pass/fail verdicts and the integrated change. The
/// fact is the UI/reporting projection of the run's verification state — the
/// production authorization for completion steps is exclusively the
/// immutable verification-record proof, never this advisory row.
/// The durable fact kind/key under which one orchestrated run records the
/// EXACT verification attempt its root settlement consumes. Keyed by run id
/// so two runs of the same parent session never share an attempt.
const ROOT_ATTEMPT_FACT_KIND: &str = "verification_attempt";

fn root_attempt_op_key(run_id: &str) -> String {
    format!("root:{run_id}")
}

/// The exact attempt op persisted for `run_id` (None before the first
/// attempt of the run — never a guess). FIX 2: a failed store read and a
/// PRESENT-but-malformed attempt fact are errors — collapsing either into
/// `None` would mint a superseding attempt over a live verification.
fn read_root_attempt_op(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
) -> Result<Option<u64>, ExecError> {
    let key = root_attempt_op_key(run_id);
    let facts = handle
        .memory_facts()
        .map_err(|e| ExecError::from(classify_session_read("verification-attempt facts", e)))?;
    let Some((_, _, value)) = facts
        .iter()
        .find(|(kind, k, _)| kind == ROOT_ATTEMPT_FACT_KIND && k == &key)
    else {
        return Ok(None);
    };
    let op: u64 = value.parse().map_err(|_| {
        ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("verification-attempt fact {key}"),
            detail: format!("stored attempt op {value:?} is not a u64"),
        })
    })?;
    if op == 0 {
        return Err(ExecError::from(DurableStateError::CorruptDurableState {
            what: format!("verification-attempt fact {key}"),
            detail: "stored attempt op is zero".into(),
        }));
    }
    Ok(Some(op))
}

/// Persist the exact verification attempt op of `run_id` durably BEFORE the
/// attempt settles, so every later settlement consumes that exact attempt.
fn persist_root_attempt_op(
    handle: &faktor_session::SessionHandle,
    run_id: &str,
    op: u64,
) -> Result<(), ExecError> {
    handle
        .upsert_memory_fact(
            ROOT_ATTEMPT_FACT_KIND,
            &root_attempt_op_key(run_id),
            &op.to_string(),
        )
        .map_err(|e| ExecError::Internal(format!("verification attempt fact write: {e}")))
}

fn persist_root_verification_fact(
    handle: &faktor_session::SessionHandle,
    status: &str,
    checks: &[faktor_core::state::CheckExecution],
    changed: &[String],
) -> Result<(), ExecError> {
    let last = serde_json::json!({
        "status": status,
        "checks": checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.check,
                    "passed": c.status == VerificationStatus::Passed,
                })
            })
            .collect::<Vec<_>>(),
        "changed": changed,
    });
    handle
        .upsert_memory_fact("verification", "last", &last.to_string())
        .map(|_| ())
        .map_err(|e| ExecError::Internal(format!("root verification fact write: {}", e.message)))
}

/// Deterministic 64-hex digest of a bounded string list (integration record
/// source/file coverage). FNV-1a folded under four independent seeds and
/// concatenated: stable across processes and platforms, and only used to
/// detect list drift (`source_count`/`integrated_file_count` carry the
/// exact cardinality beside it).
fn stable_list_digest(items: &[String]) -> String {
    let mut out = String::with_capacity(64);
    for seed in [
        0xcbf2_9ce4_8422_2325u64,
        0x9e37_79b9_7f4a_7c15,
        0x2545_f491_4f6c_dd1d,
        0x94d0_49bb_1331_11ebu64,
    ] {
        let mut hash = seed;
        for item in items {
            for b in item.as_bytes() {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            hash ^= 0xff;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        out.push_str(&format!("{hash:016x}"));
    }
    out
}

fn truncate_bytes(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

/// The production reviewer digest of one root verification run: the reviewer
/// identity of every PASSED reviewer-bearing criterion (independent review /
/// aggregate goal), the ROUTED provider+model of the ACTUAL review call the
/// run rests on ([`faktor_agent::IntegratedRootVerification::review_model_identity`]),
/// and a structured-payload digest per verdict — folded deterministically
/// into one `blake3:` digest. `None` when no reviewer-bearing criterion
/// passed OR when the run records no actual review-model call: an honest
/// absence, never a fabricated reviewer row (the parent session's configured
/// pair is NEVER substituted for the real reviewer).
fn reviewer_proof_basis_digest(run: &faktor_agent::IntegratedRootVerification) -> Option<String> {
    let mut rows: Vec<String> = Vec::new();
    for verdict in &run.criteria {
        if !verdict.passed {
            continue;
        }
        let identity = match &verdict.binding {
            Some(CriterionBinding::IndependentReview { reviewer_id }) => {
                format!("independent_review|{reviewer_id}")
            }
            Some(CriterionBinding::AggregateGoal) => "aggregate_goal|final-reviewer".to_string(),
            _ => continue,
        };
        rows.push(format!(
            "{}|{}|{}",
            verdict.criterion_key,
            identity,
            evidence_text_digest(verdict.evidence.as_deref())
        ));
    }
    if rows.is_empty() {
        return None;
    }
    // The ACTUAL review call's routed pair — never the parent's configured
    // pair. No recorded identity means no review-model call was attempted:
    // there is no reviewer to name, so the digest is an honest absence.
    let review_identity = run.review_model_identity.as_ref()?;
    rows.sort();
    rows.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-reviewer-basis:v2\0");
    for part in [
        review_identity.provider.as_bytes(),
        review_identity.model.as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    for row in &rows {
        hasher.update(&(row.len() as u64).to_le_bytes());
        hasher.update(row.as_bytes());
    }
    Some(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// One evidence text folded to a bounded, deterministic digest (the raw
/// evidence may be long; the digest is what the basis commits to).
fn evidence_text_digest(evidence: Option<&str>) -> String {
    match evidence {
        Some(text) => format!("blake3:{}", blake3::hash(text.as_bytes()).to_hex()),
        None => "<none>".to_string(),
    }
}

/// Every IMMUTABLE evidence contributing to a criterion PASS of one
/// verification run: the criterion's binding digest, the bound check's
/// execution digest, file-state/immutable-evidence digests, integration
/// coverage sources and the criterion's own evidence text digest. Sorted,
/// deduplicated and bounded so two identical runs produce the identical
/// list (and thereby the identical basis digest).
fn criterion_pass_evidence_digests(
    prepared: &PreparedRunIntegration,
    run: &faktor_agent::IntegratedRootVerification,
) -> Vec<String> {
    let checks_by_id: HashMap<&str, &CheckExecution> = run
        .checks
        .iter()
        .map(|check| (check.check.as_str(), check))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for verdict in run
        .criteria
        .iter()
        .filter(|verdict| verdict.passed)
        .take(faktor_session::task::MAX_PROOF_BASIS_ENTRIES)
    {
        let key = &verdict.criterion_key;
        if let Some(binding) = &verdict.binding {
            out.push(format!("binding:{key}:{}", binding.content_digest()));
            match binding {
                CriterionBinding::RequiredCheck {
                    check_id,
                    command_digest,
                } => {
                    out.push(format!("command:{key}:{command_digest}"));
                    if let Some(check) = checks_by_id.get(check_id.as_str()) {
                        out.push(format!("check:{key}:{}", check_execution_digest(check)));
                    }
                }
                CriterionBinding::FileState {
                    path,
                    expected_digest,
                } => out.push(format!("file:{key}:{path}:{expected_digest}")),
                CriterionBinding::Evidence {
                    evidence_id,
                    evidence_digest,
                } => out.push(format!("immutable:{key}:{evidence_id}:{evidence_digest}")),
                CriterionBinding::IntegrationCoverage {
                    required_work_items,
                } => {
                    for item in required_work_items {
                        out.push(format!("coverage:{key}:{item}"));
                    }
                    for source in &prepared.sources {
                        out.push(format!(
                            "source:{key}:{}:{}",
                            source.child_id, source.candidate_root_hash
                        ));
                    }
                }
                CriterionBinding::IndependentReview { reviewer_id } => {
                    out.push(format!("review:{key}:{reviewer_id}"));
                }
                CriterionBinding::AggregateGoal => {
                    out.push(format!("review:{key}:aggregate-goal"));
                }
                CriterionBinding::Unavailable { .. } => {}
            }
        }
        if let Some(evidence) = &verdict.evidence {
            out.push(format!(
                "evidence:{key}:blake3:{}",
                blake3::hash(evidence.as_bytes()).to_hex()
            ));
        }
    }
    out.sort();
    out.dedup();
    out.truncate(faktor_session::task::MAX_PROOF_BASIS_ENTRIES);
    out
}

/// The deterministic digest of one executed check row (the immutable
/// evidence a check-bound criterion pass resolves to).
fn check_execution_digest(check: &CheckExecution) -> String {
    stable_list_digest(&[
        "check".to_string(),
        check.check.clone(),
        check.program.clone(),
        check.args.join("\u{1f}"),
        format!("{:?}", check.status),
        check.exit.map(|c| c.to_string()).unwrap_or_default(),
    ])
}

#[cfg(test)]
#[path = "task_executor_tests.rs"]
mod task_executor_tests;

#[cfg(test)]
mod completion_contract_executor_tests {
    //! Adversarial covers of the executor's record-first contract seam:
    //! the default contract writes nothing, a non-default contract lands at
    //! the task's start revision BEFORE any run exists, and a second set for
    //! the same revision is a typed Conflict.
    use super::*;
    use faktor_session::{SessionHandle, SessionManager};

    fn manager() -> (tempfile::TempDir, Arc<SessionManager>) {
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, m)
    }

    fn session(m: &Arc<SessionManager>) -> SessionHandle {
        let ws = m.create_workspace("/w").unwrap();
        m.create_session(ws, "t", "ollama", "qwen3.8").unwrap()
    }

    #[test]
    fn record_first_contract_is_default_silent_and_immutable_per_revision() {
        let (_d, m) = manager();
        let handle = session(&m);
        let task_id = handle.task_id().unwrap();
        let now = handle.now_ms();
        handle
            .create_task(faktor_session::Task {
                task_id,
                session_id: handle.id(),
                goal: "g".into(),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: Vec::new(),
                budget: TaskBudget::default(),
                state: TaskState::Pending,
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
        // `None` and the explicit all-false contract write NOTHING.
        record_completion_contract(&handle, task_id, None).unwrap();
        record_completion_contract(&handle, task_id, Some(CompletionContract::default())).unwrap();
        assert!(handle
            .ledger_completion_contract(task_id.raw())
            .unwrap()
            .is_none());
        // A non-default contract lands durably at the start revision.
        let contract = CompletionContract {
            include_commit: true,
            include_push: true,
            include_pr: false,
        };
        record_completion_contract(&handle, task_id, Some(contract)).unwrap();
        let rev = handle.task_revision(task_id).unwrap();
        assert_eq!(
            handle.completion_contract(task_id).unwrap(),
            Some((rev, contract))
        );
        // A second set for the same revision is a typed Conflict.
        let err = record_completion_contract(
            &handle,
            task_id,
            Some(CompletionContract {
                include_commit: false,
                include_push: false,
                include_pr: true,
            }),
        )
        .unwrap_err();
        assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    }
}
