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
use faktor_core::authority::{
    authority_digest_hex, classify_authority_digest, AuthorityDigestKind, Fields,
    DOMAIN_CANDIDATE_MANIFEST, DOMAIN_CHANGED_FILES, DOMAIN_CHECK_BASIS, DOMAIN_CHECK_EXECUTION,
    DOMAIN_INTEGRATION_SOURCES, DOMAIN_RUN_BASE_MANIFEST, DOMAIN_TASK_CONTRACT,
};
use faktor_core::cancellation::CancellationToken;
use faktor_core::completion::CompletionContract;
use faktor_core::hash::FileHash;
use faktor_core::id::{OpId, SessionId, TaskId, TaskRevision, VerificationRecordId, WorktreeId};
use faktor_core::state::{
    command_binding_digest_parts, CandidateProofRef, CheckExecution, CriterionBinding,
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
    /// The request digest of the submission-keyed admission that produced
    /// this run (audit P1): admission recovery compares it with the pending
    /// row's stored digest and lands a typed key-reuse conflict on a
    /// mismatch instead of replaying the wrong receipt. `None` on unkeyed
    /// runs and legacy rows (field-level serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_digest: Option<String>,
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

/// Bound and shape of a client submission id (the idempotency key of ONE
/// logical task start): 1..=64 ASCII bytes of `[0-9a-f-]` (UUID-shaped,
/// lowercase hex only). The native wire validates this at the DTO boundary;
/// [`TaskRunRequest::validate`] enforces the same predicate for every
/// programmatic caller so no start can carry a malformed key.
pub const MAX_SUBMISSION_ID_BYTES: usize = 64;

/// True when `id` is a well-shaped client submission id (see
/// [`MAX_SUBMISSION_ID_BYTES`]).
pub fn valid_submission_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SUBMISSION_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

/// One native task start: a goal plus one or more work items. Dispatch is
/// by item count: one item drives the existing session (single-agent case),
/// two or more spawn real children through the orchestrator runtime.
#[derive(Debug, Clone)]
pub struct TaskRunRequest {
    pub goal: String,
    pub work_items: Vec<WorkItem>,
    /// The client's submission UUID of this logical start (idempotency
    /// finding 1): `Some` claims a durable admission row BEFORE any task
    /// mutation and completes it with the exact run receipt once the run was
    /// accepted, so a repeated key replays that receipt byte-for-byte and a
    /// different request under the same key is a typed KeyReused conflict.
    /// `None` (programmatic/test callers) is the unkeyed legacy start.
    pub submission_id: Option<String>,
    /// Model selector used for the single-item drive / child default.
    pub model: Option<String>,
    /// A PREALLOCATED operation id for a single-item run (audit P1): the
    /// ordinary-prompt path (`crates/server/src/native/session.rs`) reserves
    /// the id durably with its `prompt_admission` claim and hands it here, so
    /// the admitted turn journals exactly the op id the claim's reservation
    /// names. `None` (every keyed task start allocates its own inside
    /// `begin_start_admission`) leaves the historical allocation untouched.
    pub reserved_op_id: Option<OpId>,
    /// The request digest of the ORIGINATING admission when this start is
    /// driven by the ordinary-prompt path: recorded on the run row so
    /// admission recovery can detect a tampered linkage row (a mismatch is a
    /// typed conflict, never a wrong replay). `None` for unkeyed starts.
    pub admission_digest: Option<String>,
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
            submission_id: None,
            model: None,
            reserved_op_id: None,
            admission_digest: None,
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
        if let Some(submission_id) = &self.submission_id {
            if !valid_submission_id(submission_id) {
                return Err(ExecError::Malformed(format!(
                    "submission_id must be 1..={MAX_SUBMISSION_ID_BYTES} ASCII [0-9a-f-] characters"
                )));
            }
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
/// Serde round-trips are byte-stable: a submission-keyed replay returns the
/// EXACT JSON the first success stored.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskRunReceipt {
    pub run_id: String,
    pub mode: TaskRunMode,
    pub op_id: Option<OpId>,
    pub queued: bool,
}

/// The durable admission of ONE submission-keyed task start (idempotency
/// finding 1 + audit P1): the client submission key claimed in
/// `task_admission` before any task mutation of this start, together with
/// the reserved operation id whose `reservation` (`tx-<op>` / `run-<op>`)
/// links the claim to the accepted durable facts and fences completion.
struct TaskStartAdmission {
    key: String,
    digest: String,
    reservation: String,
    op_id: OpId,
}

/// Which durable run shape a submission-keyed start will admit: selects the
/// reservation prefix that recovery classification understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartAdmissionKind {
    /// One in-session run: reservation `tx-<op>`.
    InSession,
    /// One orchestrated run: reservation `run-<op>`.
    Orchestrated,
}

impl StartAdmissionKind {
    fn reservation(self, op: OpId) -> String {
        match self {
            Self::InSession => format!("tx-{:016x}", op.raw()),
            Self::Orchestrated => format!("run-{:016x}", op.raw()),
        }
    }
}

/// The admission decision of [`TaskExecutor::start_task`] for one start.
enum StartAdmission {
    /// Proceed. `Some` = an admission row was claimed for this start
    /// (complete it at acceptance / release it on a pre-acceptance
    /// failure); `None` = the caller carried no submission id (unkeyed
    /// programmatic/test start).
    Fresh(Option<TaskStartAdmission>),
    /// The submission key already completed: replay the stored receipt
    /// without touching anything.
    Replay(TaskRunReceipt),
}

/// The canonical request digest of ONE submission-keyed start: a stable
/// JSON serialization of the normalized start inputs (goal, files, the
/// effective criteria/attachment patches, the completion contract and the
/// run envelope) under a domain-separated BLAKE3. Computed on the caller's
/// thread, never on the store's writer owner.
pub(crate) fn task_start_digest(req: &TaskRunRequest) -> Result<String, ExecError> {
    #[derive(serde::Serialize)]
    struct DigestInput<'a> {
        goal: &'a str,
        files: &'a [String],
        attachments: &'a [AttachmentId],
        criteria: Option<&'a Vec<String>>,
        completion_contract: Option<CompletionContract>,
        model: Option<&'a str>,
        max_tokens: Option<u64>,
        max_cost_micro: Option<u64>,
        work_items: &'a [WorkItem],
    }
    let attachments = req.effective_attachments_patch().unwrap_or_default();
    let criteria = req.effective_criteria_patch();
    let input = DigestInput {
        goal: &req.goal,
        files: &req.files,
        attachments: &attachments,
        criteria: criteria.as_ref(),
        completion_contract: req.completion_contract,
        model: req.model.as_deref(),
        max_tokens: req.max_tokens,
        max_cost_micro: req.max_cost_micro,
        work_items: &req.work_items,
    };
    let bytes = serde_json::to_vec(&input)
        .map_err(|e| ExecError::Internal(format!("task start digest serialization: {e}")))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.task-start-admission/v1");
    hasher.update(&bytes);
    Ok(hasher.finalize().to_hex().to_string())
}

/// Claim the durable admission of ONE submission-keyed start: the FIRST
/// durable act of the start, before any shadow/task/budget/prompt work. An
/// equal-digest completion replays the stored receipt; a live pending row
/// and a digest/session mismatch are typed conflicts; a STALE pending row
/// (audit P1: a previous boot's owner or an expired lease) is classified
/// against its durable facts and re-claimed once — never a perpetual
/// in-flight refusal.
fn begin_start_admission(
    session: &Arc<SessionManager>,
    parent: SessionId,
    req: &TaskRunRequest,
    kind: StartAdmissionKind,
    now: i64,
) -> Result<StartAdmission, ExecError> {
    let Some(key) = req.submission_id.as_deref() else {
        return Ok(StartAdmission::Fresh(None));
    };
    let digest = task_start_digest(req)?;
    for _attempt in 0..2 {
        // Reserve the run's op id BEFORE the claim: its reservation becomes
        // the durable linkage recovery classifies against. A replay/in-flight
        // answer wastes only a sequence slot (never a held operation).
        let op_id = session
            .try_next_op_id()
            .map_err(|e| ExecError::from(faktor_core::Error::from(e)))?;
        let reservation = kind.reservation(op_id);
        let claim = session
            .store()
            .task_admission_claim(parent, key, &digest, &reservation, now)
            .map_err(|e| ExecError::Internal(format!("submission admission claim: {e}")))?;
        if let Some(receipt_json) = claim.complete_receipt() {
            let receipt: TaskRunReceipt = serde_json::from_str(receipt_json).map_err(|e| {
                ExecError::Internal(format!(
                    "stored run receipt of submission {key:?} did not decode: {e}"
                ))
            })?;
            return Ok(StartAdmission::Replay(receipt));
        }
        if claim.is_in_flight() {
            return Err(ExecError::Conflict(format!(
                "task start with submission id {key:?} is already in flight; retry once it settles"
            )));
        }
        if let Some(stored_digest) = claim.key_reused_digest() {
            return Err(ExecError::Conflict(format!(
                "submission id {key:?} was already used for a different task start (stored request digest {stored_digest}, this request digest {digest}); use a fresh submission id for a new start"
            )));
        }
        if let Some(row) = claim.stale_row() {
            // Audit P1: land the stale row exactly once (replay from the
            // durable facts, safe reclaim, or typed conflict) and retry the
            // claim once. A row that moved on re-claims below.
            let row = row.clone();
            land_task_admission_recovery(session, &row)?;
            continue;
        }
        debug_assert!(claim.is_fresh(), "unknown admission claim outcome");
        return Ok(StartAdmission::Fresh(Some(TaskStartAdmission {
            key: key.to_owned(),
            digest,
            reservation,
            op_id,
        })));
    }
    Err(ExecError::Conflict(format!(
        "task start with submission id {key:?} could not be resolved from its stale claim; retry once it settles"
    )))
}

/// READ-ONLY pre-run decision for one submission-keyed start: `Ok(None)`
/// means no row exists yet (a fresh claim may proceed); `Ok(Some(receipt))`
/// is the stored replay; a live pending row and digest-mismatched keys are
/// typed refusals. A STALE pending row is resolved against its durable facts
/// first (audit P1), never answered as in-flight. Never writes an admission
/// claim: the authoritative claim is [`begin_start_admission`].
fn peek_start_admission(
    session: &Arc<SessionManager>,
    parent: SessionId,
    req: &TaskRunRequest,
    now: i64,
) -> Result<Option<TaskRunReceipt>, ExecError> {
    let Some(key) = req.submission_id.as_deref() else {
        return Ok(None);
    };
    let digest = task_start_digest(req)?;
    let Some(claim) = session
        .store()
        .task_admission_peek(parent, key, &digest, now)
        .map_err(|e| ExecError::Internal(format!("submission admission peek: {e}")))?
    else {
        return Ok(None);
    };
    if let Some(receipt_json) = claim.complete_receipt() {
        let receipt: TaskRunReceipt = serde_json::from_str(receipt_json).map_err(|e| {
            ExecError::Internal(format!(
                "stored run receipt of submission {key:?} did not decode: {e}"
            ))
        })?;
        return Ok(Some(receipt));
    }
    if claim.is_in_flight() {
        return Err(ExecError::Conflict(format!(
            "task start with submission id {key:?} is already in flight; retry once it settles"
        )));
    }
    if let Some(stored_digest) = claim.key_reused_digest() {
        return Err(ExecError::Conflict(format!(
            "submission id {key:?} was already used for a different task start (stored request digest {stored_digest}, this request digest {digest}); use a fresh submission id for a new start"
        )));
    }
    if let Some(row) = claim.stale_row() {
        // Resolve the stale row now; the authoritative claim below decides
        // between the recovered replay and a fresh execution.
        let row = row.clone();
        land_task_admission_recovery(session, &row)?;
        return Ok(None);
    }
    debug_assert!(claim.is_fresh(), "unknown admission peek outcome");
    Ok(None)
}

/// Complete one claimed admission with the byte-exact serialized receipt,
/// fenced by the claim's reservation.
fn complete_start_admission(
    session: &SessionManager,
    parent: SessionId,
    admission: Option<&TaskStartAdmission>,
    receipt: &TaskRunReceipt,
) -> Result<(), ExecError> {
    let Some(admission) = admission else {
        return Ok(());
    };
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| ExecError::Internal(format!("run receipt serialization: {e}")))?;
    session
        .store()
        .task_admission_complete(
            parent,
            &admission.key,
            &admission.reservation,
            &receipt_json,
        )
        .map_err(|e| ExecError::Internal(format!("submission admission complete: {e}")))
}

/// Release a claimed admission that failed BEFORE acceptance (a completed
/// receipt is never deleted; the store's release only touches the pending
/// row still owned by this reservation).
fn release_start_admission(
    session: &SessionManager,
    parent: SessionId,
    admission: Option<&TaskStartAdmission>,
) {
    let Some(admission) = admission else {
        return;
    };
    if let Err(e) =
        session
            .store()
            .task_admission_release(parent, &admission.key, &admission.reservation)
    {
        eprintln!(
            "submission admission release for session {parent} failed (a retry may answer in flight): {e}"
        );
    }
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
    /// the root. Idempotent for the same (session, run). An UNCONFIGURED
    /// root (the store was opened without an explicit data root) is a typed
    /// refusal: a candidate root is NEVER guessed from a fixed temp path.
    pub fn allocate(&self, session: SessionId, run_id: &str) -> Result<PathBuf, ExecError> {
        if self.root.as_os_str().is_empty() {
            return Err(ExecError::InvalidPlan(
                "configuration: candidate run root is unconfigured (the session store was opened without an explicit data root); refusing to allocate an isolated run root".into(),
            ));
        }
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

/// The runtime OWNER of every detached drive the executor spawns.
///
/// Audited gap: the drives dispatched by [`TaskExecutor::start_task`],
/// [`TaskExecutor::resume_run`] and the bounded shadow watcher were
/// `tokio::spawn`ed with their `JoinHandle`s dropped — no runtime value
/// owned them, so a shutdown could neither await nor cancel an in-flight
/// drive and a drive could outlive every handle the daemon holds. This
/// registry is that owner: it holds each handle in a [`JoinSet`] (dropping
/// the registry aborts every remaining drive by `JoinSet` drop semantics),
/// tracks the run id each live drive serves, and offers a bounded
/// graceful-then-abort shutdown:
///
/// 1. the registry closes synchronously — no drive can start after shutdown
///    began (`spawn` refuses, and the caller keeps the run's durable rows
///    resumable instead of pretending it was driven);
/// 2. in-flight drives get `grace` to finish their record-first durable
///    writes and settlement;
/// 3. every straggler is aborted and reaped within [`Self::ABORT_REAP_GRACE`].
///
/// An abort is crash-equivalent BY CONSTRUCTION: every drive commits its
/// durable markers (plan/assignment/registry/settlement rows) BEFORE the
/// guarded side effect, so a drive cut mid-flight is recovered by
/// `resume_run`/the deterministic settlement — no durable write is ever
/// silently dropped, only deferred to the durable recovery authority.
pub struct TaskDriveRegistry {
    inner: Mutex<TaskDriveRegistryInner>,
}

struct TaskDriveRegistryInner {
    tasks: tokio::task::JoinSet<()>,
    labels: HashMap<tokio::task::Id, String>,
    closed: bool,
}

/// Bounded shutdown outcome of [`TaskDriveRegistry::shutdown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriveDrainReport {
    /// Drives owned when shutdown began.
    pub total: usize,
    /// Drives that finished during the graceful window.
    pub completed: usize,
    /// Drives that had to be aborted after the graceful window.
    pub aborted: usize,
    /// Aborted drives NOT reaped within
    /// [`TaskDriveRegistry::ABORT_REAP_GRACE`] (their handles were dropped;
    /// the durable rows remain the recovery authority).
    pub unreaped: usize,
}

impl DriveDrainReport {
    /// TRUE when every owned drive was reaped within the bound.
    pub fn all_reaped(&self) -> bool {
        self.unreaped == 0
    }
}

impl Default for TaskDriveRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskDriveRegistry {
    /// Abort/reap bound after the graceful window elapsed.
    pub const ABORT_REAP_GRACE: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        Self {
            inner: Mutex::new(TaskDriveRegistryInner {
                tasks: tokio::task::JoinSet::new(),
                labels: HashMap::new(),
                closed: false,
            }),
        }
    }

    /// Poison-tolerant guard: the registry is a liveness authority and must
    /// stay drainable even after a panic elsewhere (the state it protects is
    /// only handles + labels).
    fn lock(&self) -> std::sync::MutexGuard<'_, TaskDriveRegistryInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            self.inner.clear_poison();
            poisoned.into_inner()
        })
    }

    /// Reap every finished drive and its label. A panicked drive is logged
    /// and reaped like any other, so a panic can never wedge the registry.
    fn reap_finished(inner: &mut TaskDriveRegistryInner) {
        while let Some(result) = inner.tasks.try_join_next_with_id() {
            match result {
                Ok((id, ())) => {
                    inner.labels.remove(&id);
                }
                Err(e) => {
                    let label = inner.labels.remove(&e.id()).unwrap_or_default();
                    if e.is_panic() {
                        tracing::error!(run = %label, "detached drive task panicked: {e}");
                    } else {
                        tracing::warn!(run = %label, "detached drive task was cancelled: {e}");
                    }
                }
            }
        }
    }

    /// Spawn one detached drive owned by this registry. `false` = the
    /// registry is already shut down: the drive was NOT started and the
    /// caller must keep the durable run resumable (and free its in-memory
    /// slot). New work after shutdown is refused, never orphaned.
    #[must_use]
    pub fn spawn(
        &self,
        run_id: impl Into<String>,
        drive: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        let label = run_id.into();
        let mut inner = self.lock();
        Self::reap_finished(&mut inner);
        if inner.closed {
            return false;
        }
        let abort = inner.tasks.spawn(drive);
        inner.labels.insert(abort.id(), label);
        true
    }

    /// The number of drives RUNNING right now (finished drives are reaped
    /// first, so this is a live count, never a completed high-water mark).
    pub fn live_task_count(&self) -> usize {
        let mut inner = self.lock();
        Self::reap_finished(&mut inner);
        inner.tasks.len()
    }

    /// The run ids of the currently running drives, sorted (tests/health).
    pub fn live_run_ids(&self) -> Vec<String> {
        let mut inner = self.lock();
        Self::reap_finished(&mut inner);
        let mut ids: Vec<String> = inner.labels.values().cloned().collect();
        ids.sort();
        ids
    }

    /// TRUE once [`Self::shutdown`] began (new spawns are refused).
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }

    /// Bounded shutdown: close, give in-flight drives `grace` to finish,
    /// then abort and reap the stragglers within [`Self::ABORT_REAP_GRACE`].
    /// Idempotent; a second call finds nothing to drain.
    pub async fn shutdown(&self, grace: Duration) -> DriveDrainReport {
        let (mut tasks, mut labels) = {
            let mut inner = self.lock();
            inner.closed = true;
            (
                std::mem::take(&mut inner.tasks),
                std::mem::take(&mut inner.labels),
            )
        };
        let total = tasks.len();
        let mut completed = 0usize;
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            match tokio::time::timeout_at(deadline, tasks.join_next_with_id()).await {
                Ok(Some(Ok((id, ())))) => {
                    labels.remove(&id);
                    completed += 1;
                }
                Ok(Some(Err(e))) => {
                    labels.remove(&e.id());
                    if e.is_panic() {
                        tracing::error!("owned drive task reaped with a panic: {e}");
                    }
                    completed += 1;
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        let mut aborted = 0usize;
        let mut unreaped = 0usize;
        if !tasks.is_empty() {
            aborted = tasks.len();
            let mut aborted_runs: Vec<String> = labels.values().cloned().collect();
            aborted_runs.sort();
            tracing::warn!(
                aborted,
                runs = %aborted_runs.join(","),
                "drive shutdown grace elapsed; aborting in-flight drives (durable markers keep them resumable)"
            );
            tasks.abort_all();
            let reap_deadline = tokio::time::Instant::now() + Self::ABORT_REAP_GRACE;
            loop {
                match tokio::time::timeout_at(reap_deadline, tasks.join_next()).await {
                    Ok(Some(Ok(()))) | Ok(Some(Err(_))) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            unreaped = tasks.len();
            if unreaped > 0 {
                tracing::error!(
                    unreaped,
                    "drive tasks did not reap after abort; their handles were dropped (durable rows remain the recovery authority)"
                );
            }
        }
        DriveDrainReport {
            total,
            completed,
            aborted,
            unreaped,
        }
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
/// True when this (run, message) pair has not been noted before. Shared by
/// the unavailable-root-verification dedup: an unchanged refusal logs once.
pub(crate) fn note_once(notes: &mut HashMap<String, String>, key: &str, message: &str) -> bool {
    if notes.get(key).map(String::as_str) == Some(message) {
        return false;
    }
    notes.insert(key.to_string(), message.to_string());
    true
}

pub struct TaskExecutor {
    orchestrator: Arc<OrchestratorRuntime>,
    session: Arc<SessionManager>,
    agent: Arc<AgentRuntime>,
    /// Active orchestrated runs by run id (audits 7/8/21/22): ONE run per
    /// parent session (per-session sequential), while runs of different
    /// parent sessions run concurrently through the runtime's run-scoped
    /// mirrors.
    active: Mutex<HashMap<String, ActiveRun>>,
    /// Deduplicated loudness for permanently-unavailable root verification:
    /// the shadow watcher re-settles on a backoff, and an unchanged reason
    /// must not be reprinted on every pass.
    verification_notes: Mutex<HashMap<String, String>>,
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
    /// Test-only seam (P1): makes the post-admission session read fail so
    /// the typed "accepted but recovery-required" contract can be proven
    /// without a real store outage.
    #[cfg(test)]
    post_admission_read_seam: std::sync::atomic::AtomicBool,
    /// The runtime OWNER of every detached drive this executor spawns
    /// ([`TaskDriveRegistry`]): handles are held (never dropped), live task
    /// ids are tracked, and [`TaskExecutor::shutdown_drives`] drains them
    /// with a bounded graceful-then-abort window.
    drives: Arc<TaskDriveRegistry>,
}

/// The completion-step wiring of one executor: the configured template
/// values, the injected canonical SCM adapter of the native PR step (the
/// daemon wires `faktor_scm::GitHubCompletionScm` over the real GitHub App;
/// `None` = a contracted PR step records an explicit configuration blocker)
/// and the cached runner (rebuilt when the config or the adapter changes).
#[derive(Default)]
struct CompletionStepsWiring {
    config: CompletionStepsConfig,
    scm: Option<Arc<dyn faktor_scm::CompletionScm>>,
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
    /// Print an unavailable-root-verification note ONCE per unchanged run
    /// reason: the shadow watcher re-settles on a bounded backoff and an
    /// identical refusal must not flood the log.
    fn note_root_verification_unavailable(&self, run_id: &str, message: &str) {
        let mut notes = self
            .verification_notes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if notes.len() > 1024 {
            notes.clear();
        }
        if note_once(&mut notes, run_id, message) {
            eprintln!("{message}");
        }
    }

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
            #[cfg(test)]
            post_admission_read_seam: std::sync::atomic::AtomicBool::new(false),
            orchestrator: orchestrator.clone(),
            session,
            agent,
            active: Mutex::new(HashMap::new()),
            verification_notes: Mutex::new(HashMap::new()),
            shadows,
            run_roots,
            completion_steps: Mutex::new(CompletionStepsWiring::default()),
            settlement_seam: Mutex::new(None),
            placement: Mutex::new(WorkerPlacement::disabled()),
            drives: Arc::new(TaskDriveRegistry::new()),
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
                let mut manifest_fields = Fields::new().uint(manifest.entries().len() as u64);
                for entry in manifest.entries() {
                    manifest_fields = manifest_fields
                        .text(&entry.normalized_path)
                        .text(&entry.payload_digest.to_string());
                }
                let record = faktor_session::ledger::RunBaseRecord {
                    run_id: run_id.to_string(),
                    workspace_id,
                    worktree_id,
                    snapshot_hash: copied,
                    manifest_digest: authority_digest_hex(
                        DOMAIN_RUN_BASE_MANIFEST,
                        1,
                        manifest_fields,
                    ),
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

    /// Install (or clear) the canonical SCM adapter of the native PR step
    /// (`faktor_scm::GitHubCompletionScm` over the real GitHub App adapter in
    /// production). The cached runner is dropped so the next contracted run
    /// rebuilds with it. Without an adapter a task whose completion contract
    /// requests the PR step records the explicit
    /// `faktor-orchestrator` configuration blocker instead of silently
    /// skipping it.
    pub fn set_completion_scm_provider(
        &self,
        provider: Option<Arc<dyn faktor_scm::CompletionScm>>,
    ) {
        let mut wiring = self.lock_completion_steps();
        wiring.scm = provider;
        wiring.runner = None;
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
            // Fail closed (audit P2): an absent sandbox is never an egress
            // grant — a completion step that needs the network is refused
            // typed, and local/file:// remotes never consult this policy.
            None => Err(
                "no sandbox configured: refusing to execute a completion step that requires egress"
                    .to_string(),
            ),
        });
        let runner = Arc::new(
            CompletionStepRunner::new(supervisor, egress, wiring.config.clone())
                .map_err(|e| ExecError::InvalidPlan(format!("completion-step runner config: {e}")))?
                .with_scm_provider_or_none(wiring.scm.clone()),
        );
        wiring.runner = Some(runner.clone());
        Ok(Some(runner))
    }

    /// The default candidate-root authority: `<store data root>/candidate-runs`.
    /// [`faktor_store::Store::root`] is the EXPLICIT data root the store was
    /// opened at (the directory of `faktor-plus.db`). There is NO fixed
    /// temp-dir fallback: a degenerate empty root refuses allocation typed
    /// ([`ExecError::InvalidPlan`]) instead of writing under a shared `/tmp`
    /// path, so no test or embedded host can ever inherit a production
    /// temp fallback.
    fn default_run_roots(session: &SessionManager) -> Arc<CandidateWorkspaceService> {
        CandidateWorkspaceService::new(session.store().root().join("candidate-runs"))
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

    /// The number of detached drives this executor currently owns
    /// (tests/health): finished drives are reaped first, so this is a live
    /// count.
    pub fn live_drive_count(&self) -> usize {
        self.drives.live_task_count()
    }

    /// The run ids of the detached drives this executor currently owns,
    /// sorted (tests/health).
    pub fn live_drive_runs(&self) -> Vec<String> {
        self.drives.live_run_ids()
    }

    /// Whether the drive registry is closed (shutdown began): every later
    /// start/resume is refused typed instead of silently detached.
    pub fn drives_shutdown(&self) -> bool {
        self.drives.is_closed()
    }

    /// Bounded, deterministic shutdown of every detached drive this executor
    /// owns: close the registry (refusing new drives), give in-flight drives
    /// `grace` to finish their record-first durable writes/settlement, then
    /// abort and reap the stragglers within
    /// [`TaskDriveRegistry::ABORT_REAP_GRACE`]. Shutdown is TERMINAL for the
    /// executor: an aborted run stays recoverable and is reconstructed from
    /// its durable rows by a fresh executor (`resume_run`).
    pub async fn shutdown_drives(&self, grace: Duration) -> DriveDrainReport {
        self.drives.shutdown(grace).await
    }

    /// P1 test seam: make the NEXT post-admission session read fail.
    #[cfg(test)]
    pub(crate) fn fail_post_admission_read_for_test(&self) {
        self.post_admission_read_seam
            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        // Idempotency finding 1: the submission-keyed decision is made
        // BEFORE every pre-run step (attachment resolution, identity
        // adoption, live-run blockers, worker placement, shadow
        // settlement). A completed key replays the stored receipt with zero
        // mutation; a pending or digest-mismatched key refuses typed. A
        // fresh key still CLAIMS inside `start_in_session` /
        // `start_orchestrated` (that claim is authoritative; this peek only
        // keeps duplicates from touching pre-run state).
        // Admission time is the MONOTONIC clock: a wall-clock jump never
        // moves a lease (P1-IDEMPOTENCY).
        if let Some(receipt) =
            peek_start_admission(&self.session, parent, &req, self.session.admission_now_ms())?
        {
            return Ok(receipt);
        }
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
            self.start_in_session(parent, &handle, req)
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
        // Registry-owned drive: never a dropped JoinHandle (audit spawn
        // ownership). A closed registry refuses BEFORE the detached future
        // starts; the durable rows stay resumable and the in-memory slot is
        // freed, never left dangling behind a receipt nobody drives.
        let spawned = self.drives.spawn(run_id_owned.clone(), {
            let run_id_owned = run_id_owned.clone();
            async move {
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
            }
        });
        if !spawned {
            self.clear_active_if(parent, &run_id_owned);
            return Err(ExecError::Conflict(format!(
                "executor is shut down; run '{run_id_owned}' was not re-driven and stays resumable from its durable rows"
            )));
        }
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

    /// The submission-keyed wrapper (idempotency finding 1): the durable
    /// admission decision is the FIRST act of a keyed start, BEFORE any
    /// shadow/task/budget/prompt work. A completed key replays the stored
    /// receipt byte-for-byte; a pending or digest-mismatched key is a typed
    /// conflict; only a fresh claim proceeds into the admitted body.
    fn start_in_session(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
        req: TaskRunRequest,
    ) -> Result<TaskRunReceipt, ExecError> {
        let admission = match begin_start_admission(
            &self.session,
            parent,
            &req,
            StartAdmissionKind::InSession,
            self.session.admission_now_ms(),
        )? {
            StartAdmission::Replay(receipt) => return Ok(receipt),
            StartAdmission::Fresh(admission) => admission,
        };
        let mut accepted = false;
        let result =
            self.start_in_session_admitted(parent, handle, req, admission.as_ref(), &mut accepted);
        if result.is_err() && !accepted {
            release_start_admission(&self.session, parent, admission.as_ref());
        }
        result
    }

    /// The admitted body of ONE in-session start: the existing single-item
    /// drive, wrapped so acceptance is durably admitted (the wrapper above
    /// owns the claim/replay/release decision).
    fn start_in_session_admitted(
        self: &Arc<Self>,
        parent: SessionId,
        handle: &faktor_session::SessionHandle,
        req: TaskRunRequest,
        admission: Option<&TaskStartAdmission>,
        accepted: &mut bool,
    ) -> Result<TaskRunReceipt, ExecError> {
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
            Some(self.owner_root_of(parent, handle)?)
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
                    return self.admit_refused_isolation(
                        parent, handle, &req, refusal, admission, accepted,
                    );
                }
                return Err(refusal);
            }
        }
        // Durable task row (wave 9/16): one row per session task. A fresh
        // session seeds with the run's goal; a non-terminal existing row is
        // re-goaled; a TERMINAL row is frozen (the task certified its
        // lifetime) — a new task needs a fresh session.
        let mut task_id = handle.task_id()?;
        let now = handle.now_ms();
        let goal = truncate_bytes(&req.goal, MAX_TASK_GOAL_BYTES);
        let mut existing = handle.get_task(task_id)?;
        // Multi-turn sessions: a completion-relevant or terminal row belongs
        // to the run that CLOSED it. A new prompt opens the NEXT task row on
        // the same session instead of re-goaling a row the provider-op gate
        // refuses (or 409ing "start a fresh session" for a chat turn).
        if existing
            .as_ref()
            .is_some_and(|t| t.state.is_completion_relevant() || t.state.is_terminal())
        {
            let next = faktor_core::id::TaskId::new(task_id.raw() + 1);
            let worktree = handle
                .worktree_id()
                .map_err(|e| ExecError::Internal(format!("session worktree id: {e}")))?;
            self.session
                .adopt_identity(parent, worktree, next)
                .map_err(|e| ExecError::Internal(format!("task identity advance: {e}")))?;
            task_id = next;
            existing = None;
        }
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
                    .map_err(|e| {
                        // Two admissions racing the same next id: the loser
                        // answers a typed 409, never a 500.
                        let message = format!("task row seed: {e}");
                        if message.contains("already exists") {
                            ExecError::Conflict(message)
                        } else {
                            ExecError::Internal(message)
                        }
                    })?;
            }
        }
        // P2 record-first: the accepted completion contract lands durably
        // BEFORE the run's first model call (the submit below drives it).
        record_completion_contract(handle, task_id, req.completion_contract)?;
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
        // Audit P1: the turn journals the PREALLOCATED operation id the
        // admission (keyed task start) or the originating prompt-admission
        // claim reserved, so the claim's reservation names exactly this turn.
        let reserved_op_id = admission.map(|a| a.op_id).or(req.reserved_op_id);
        debug_assert!(
            admission.is_none() || req.reserved_op_id.is_none(),
            "a keyed start and an origin reservation never coexist"
        );
        let admission_digest = admission
            .map(|a| a.digest.clone())
            .or_else(|| req.admission_digest.clone());
        let receipt = self
            .agent
            .submit_with_op_id(parent, &req.goal, &req.files, reserved_op_id)
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
            submission_digest: admission_digest,
        };
        put_run_row(handle, &run_id, &row)?;
        // P0 no-op policy: the run's disposition is durable BEFORE the drive
        // is dispatched. The shadow settlement reads it under the synthetic
        // per-session run id (`tx-session-<session>`), so the row is written
        // under BOTH the real run id and that id.
        write_run_policy_row(handle, &run_id, req.no_op_disposition)?;
        write_run_policy_row(
            handle,
            &format!("tx-session-{}", parent.raw()),
            req.no_op_disposition,
        )?;
        // Acceptance: the prompt is durably recorded and the run's linkage +
        // policy rows exist. The exact receipt is admitted BEFORE the spawn
        // attempt, so a shut-down refusal below keeps the completed receipt
        // (a same-key retry replays it instead of starting a second run).
        let run_receipt = TaskRunReceipt {
            run_id: run_id.clone(),
            mode: TaskRunMode::InSession,
            op_id: Some(receipt.op_id),
            queued: receipt.queued,
        };
        *accepted = true;
        complete_start_admission(&self.session, parent, admission, &run_receipt)?;
        // Detached drive — the daemon's own entries, identical to the
        // direct prompt path (the drive runs session recovery first; an
        // interrupted drive resumes the SAME recorded turn on daemon start).
        // Shadowed runs additionally finalize the shadow once the drive
        // returns (integrate on verified-complete, discard on failure).
        let exec = self.clone();
        if receipt.queued {
            let agent = self.agent.clone();
            // Registry-owned drive (audit spawn ownership): the handle is
            // never dropped. A shut-down registry refuses here; the queue row
            // and task/run rows are already durable, so the prompt is not
            // lost — `recover_pending_queues` drains it on the next executor.
            let spawned = self.drives.spawn(run_id.clone(), async move {
                agent.run_session_queue(parent).await;
                // The runner is bounded by the turn budget; when it exits
                // with the head still pending, the settle path re-kicks it
                // here (the runtime gate arms a live runner or this spawns a
                // fresh one — exactly one drive ever claims the head).
                exec.kick_pending_queue(parent);
                exec.after_shadowed_drive(parent).await;
            });
            if !spawned {
                return Err(ExecError::Conflict(format!(
                    "executor is shut down; queued run '{run_id}' was recorded durably and resumes on the next executor"
                )));
            }
        } else {
            let agent = self.agent.clone();
            let model = req.model.clone();
            // P1: the post-admission session read is a DURABLE read. A store
            // failure (or a vanished row) must NOT silently skip the drive
            // and return an apparently-successful start: it fails loudly
            // while the already-recorded durable run stays explicitly
            // recoverable (the receipt/op row exists; the retry replays it
            // and the recovery path drives it exactly once).
            #[cfg(test)]
            let seam_tripped = self
                .post_admission_read_seam
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(test))]
            let seam_tripped = false;
            if seam_tripped {
                return Err(ExecError::Internal(format!(
                    "post-admission session read failed for {parent} (injected); the accepted run is durably recorded and recovery-required"
                )));
            }
            let handle2 = match self.session.get_session(parent) {
                Ok(Some(h)) => Some(h),
                Ok(None) => {
                    return Err(ExecError::NotFound(format!(
                        "post-admission session {parent} vanished; the accepted run is durably recorded and recovery-required"
                    )));
                }
                Err(e) => {
                    return Err(ExecError::Internal(format!(
                        "post-admission session read failed for {parent}: {e}; the accepted run is durably recorded and recovery-required"
                    )));
                }
            };
            if let Some(h) = handle2 {
                let receipt2 = receipt.clone();
                // Registry-owned drive (audit spawn ownership): the handle is
                // never dropped. The submit already recorded the durable op
                // row, so a shut-down refusal leaves the turn recoverable.
                let spawned = self.drives.spawn(run_id.clone(), async move {
                    if let Err(e) = agent.drive_receipt(&h, receipt2, model).await {
                        eprintln!("in-session drive failed for session {parent}: {e}");
                    }
                    // Settle-path authority (boundary race): the turn just
                    // finished or was cancelled — a durable queue head that
                    // was waiting on it must never be left without a runner.
                    // A live runner is ARMED for one more bounded pass by the
                    // runtime gate; a dead one is replaced here. A shut-down
                    // registry leaves the row pending for recovery.
                    exec.kick_pending_queue(parent);
                    exec.after_shadowed_drive(parent).await;
                });
                if !spawned {
                    return Err(ExecError::Conflict(format!(
                        "executor is shut down; run '{run_id}' was recorded durably and resumes on the next executor"
                    )));
                }
            }
        }
        Ok(run_receipt)
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
        admission: Option<&TaskStartAdmission>,
        accepted: &mut bool,
    ) -> Result<TaskRunReceipt, ExecError> {
        let reserved_op_id = admission.map(|a| a.op_id).or(req.reserved_op_id);
        let admission_digest = admission
            .map(|a| a.digest.clone())
            .or_else(|| req.admission_digest.clone());
        let receipt = self
            .agent
            .submit_with_op_id(parent, &req.goal, &req.files, reserved_op_id)
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
        // The Failed event above is the durable authority; the record close
        // is compensated through the agent's retry-on-next-open channel, so
        // a close that fails here stays surfaced AND replayable instead of
        // silently leaving a stale active record.
        self.agent
            .note_turn_record_close(handle, receipt.op_id, "failed");
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
            submission_digest: admission_digest,
        };
        put_run_row(handle, &run_id, &row)?;
        // Acceptance: the refused-isolation run is durably admitted (prompt,
        // failed event, linkage row). The exact receipt is admitted so a
        // same-key retry replays it instead of admitting a second run.
        let run_receipt = TaskRunReceipt {
            run_id,
            mode: TaskRunMode::InSession,
            op_id: Some(receipt.op_id),
            queued: false,
        };
        *accepted = true;
        complete_start_admission(&self.session, parent, admission, &run_receipt)?;
        tracing::info!(
            target: "faktor::task_executor",
            session = %parent,
            "shadow phase refused (bounded caps); turn admitted and landed failed_recoverable: {message}"
        );
        Ok(run_receipt)
    }

    /// The submission-keyed wrapper of the multi-item path (idempotency
    /// finding 1): same claim-first/replay/release contract as
    /// [`Self::start_in_session`]; acceptance is the drive registry
    /// accepting the detached run.
    fn start_orchestrated(
        self: &Arc<Self>,
        parent: SessionId,
        req: TaskRunRequest,
    ) -> Result<TaskRunReceipt, ExecError> {
        let handle = self
            .session
            .get_session(parent)?
            .ok_or_else(|| ExecError::NotFound(format!("session {parent}")))?;
        let admission = match begin_start_admission(
            &self.session,
            parent,
            &req,
            StartAdmissionKind::Orchestrated,
            self.session.admission_now_ms(),
        )? {
            StartAdmission::Replay(receipt) => return Ok(receipt),
            StartAdmission::Fresh(admission) => admission,
        };
        let mut accepted = false;
        let result = self.start_orchestrated_admitted(
            parent,
            handle,
            req,
            admission.as_ref(),
            &mut accepted,
        );
        if result.is_err() && !accepted {
            release_start_admission(&self.session, parent, admission.as_ref());
        }
        result
    }

    /// The admitted body of ONE orchestrated start (see
    /// [`Self::start_orchestrated`]).
    fn start_orchestrated_admitted(
        self: &Arc<Self>,
        parent: SessionId,
        handle: faktor_session::SessionHandle,
        req: TaskRunRequest,
        admission: Option<&TaskStartAdmission>,
        accepted: &mut bool,
    ) -> Result<TaskRunReceipt, ExecError> {
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
        // Audit P1: the run id is the op id the admission claim reserved
        // (`run-<op>` is the claim's durable reservation), so recovery can
        // link the pending row to the plan/registry facts of exactly this
        // run. An unkeyed start allocates as before.
        let run_id = match admission.map(|a| a.op_id).or(req.reserved_op_id) {
            Some(op_id) => StartAdmissionKind::Orchestrated.reservation(op_id),
            None => format!(
                "run-{:016x}",
                self.session
                    .try_next_op_id()
                    .map_err(|e| ExecError::from(faktor_core::Error::from(e)))?
                    .raw()
            ),
        };
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
        // Registry-owned drive (audit spawn ownership): never a dropped
        // JoinHandle. A shut-down registry refuses BEFORE the drive starts;
        // the plan/policy/assignment rows stay durable and the in-memory slot
        // is freed so recovery can re-attach through `resume_run`.
        let spawned = self.drives.spawn(run_id2.clone(), {
            let run_id2 = run_id2.clone();
            async move {
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
            }
        });
        if !spawned {
            self.clear_active_if(parent, &run_id2);
            return Err(ExecError::Conflict(format!(
                "executor is shut down; orchestrated run '{run_id2}' was not driven and stays resumable from its durable rows"
            )));
        }
        // Acceptance: the drive registry owns the detached run. The exact
        // receipt is admitted so a same-key retry replays it instead of
        // registering a second orchestrated run.
        let run_receipt = TaskRunReceipt {
            run_id,
            mode: TaskRunMode::Orchestrated,
            op_id: None,
            queued: false,
        };
        *accepted = true;
        complete_start_admission(&self.session, parent, admission, &run_receipt)?;
        Ok(run_receipt)
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
}

mod integration;
mod recovery;
mod settlement;
mod verification;
pub use recovery::*;
pub use verification::*;

#[cfg(test)]
#[path = "../task_executor_tests/mod.rs"]
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

#[cfg(test)]
mod drive_ownership_tests {
    //! Adversarial covers of the audited spawn-ownership gap: every detached
    //! drive is owned by [`TaskDriveRegistry`], shutdown aborts/awaits within
    //! bound and leaves the durable rows recoverable, and a shut-down
    //! registry refuses new drives instead of orphaning them.
    //!
    //! (the suite-level HEAVY_SUITE guard is held across awaits BY DESIGN:
    //! it serializes the file/CAS/SQLite-heavy executor fixture with the
    //! other heavy suites.)
    #![allow(clippy::await_holding_lock)]

    use super::*;
    use crate::runtime::Ceilings;
    use crate::test_support::heavy_guard;
    use faktor_agent::{AgentDeps, NoEvidence, PermissionRequester, ToolCallMode, ToolRegistry};
    use faktor_core::capability::PermissionDecision;
    use faktor_core::model::ModelCapabilities;
    use faktor_core::time::SystemClock;
    use faktor_provider::{
        GenericAgentRequest, Provider, ProviderChunk, ProviderRegistry, ProviderStream,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    async fn wait_until(mut cond: impl FnMut() -> bool, timeout_secs: u64) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "wait_until timed out after {timeout_secs}s"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // ---------------------------------------------------- registry (unit)

    /// Shutdown must abort and reap every parked drive within bound, empty
    /// the registry, and refuse any later drive (no post-shutdown orphan).
    #[tokio::test]
    async fn drive_registry_shutdown_aborts_reaps_and_refuses_new_work() {
        let registry = TaskDriveRegistry::new();
        let entered = Arc::new(AtomicUsize::new(0));
        for i in 0..3 {
            let entered = entered.clone();
            assert!(registry.spawn(format!("run-{i}"), async move {
                entered.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }));
        }
        assert_eq!(registry.live_task_count(), 3);
        assert_eq!(registry.live_run_ids(), vec!["run-0", "run-1", "run-2"]);
        assert!(!registry.is_closed());
        let started = Instant::now();
        let report = registry.shutdown(Duration::from_millis(50)).await;
        assert!(started.elapsed() < Duration::from_secs(5), "bounded drain");
        assert_eq!(report.total, 3);
        assert_eq!(report.completed, 0);
        assert_eq!(report.aborted, 3);
        assert_eq!(report.unreaped, 0);
        assert!(report.all_reaped());
        assert_eq!(registry.live_task_count(), 0);
        assert!(registry.live_run_ids().is_empty());
        assert!(registry.is_closed());
        // A drive handed in after shutdown is REFUSED, never orphaned.
        let late = Arc::new(AtomicBool::new(false));
        let late_flag = late.clone();
        assert!(!registry.spawn("run-late", async move {
            late_flag.store(true, Ordering::SeqCst);
        }));
        assert_eq!(registry.live_task_count(), 0);
        tokio::task::yield_now().await;
        assert!(!late.load(Ordering::SeqCst), "refused drive never ran");
        // Idempotent: a second shutdown drains nothing.
        let second = registry.shutdown(Duration::from_millis(10)).await;
        assert_eq!(second.total, 0);
        assert!(second.all_reaped());
    }

    /// The graceful window is real: a drive that commits its record-first
    /// marker inside the window is allowed to land it (no premature abort),
    /// while a straggler is only aborted AFTER the window elapsed.
    #[tokio::test]
    async fn drive_registry_grace_window_is_bounded_and_aborts_only_after_it() {
        let registry = TaskDriveRegistry::new();
        let committed = Arc::new(AtomicBool::new(false));
        let committing = committed.clone();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        assert!(registry.spawn("run-commits", async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            committing.store(true, Ordering::SeqCst); // durable marker stand-in
            let _ = held.await; // guarded effect parks
        }));
        let started = Instant::now();
        let report = registry.shutdown(Duration::from_millis(250)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "grace window honored"
        );
        assert!(
            committed.load(Ordering::SeqCst),
            "in-flight durable marker landed before the abort"
        );
        assert_eq!(report.completed, 0);
        assert_eq!(report.aborted, 1);
        assert_eq!(report.unreaped, 0);
        drop(release);
    }

    /// Finished drives are reaped on the next observation: the live count is
    /// never a completed high-water mark.
    #[tokio::test]
    async fn drive_registry_reaps_finished_drives() {
        let registry = TaskDriveRegistry::new();
        assert!(registry.spawn("run-quick", async {}));
        for _ in 0..1000 {
            if registry.live_task_count() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(registry.live_task_count(), 0);
        assert!(registry.live_run_ids().is_empty());
    }

    // --------------------------------------------- executor (integration)

    struct AlwaysAllow;
    impl PermissionRequester for AlwaysAllow {
        fn request(
            &self,
            _session: SessionId,
            _permission: &faktor_session::ops::PermissionRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
        > {
            Box::pin(async { Ok(PermissionDecision::Allow) })
        }
    }

    /// Provider whose model calls PARK until the test opens the gate: the
    /// drive stays deterministically in flight across shutdown, then serves
    /// a complete turn to the resumed drive.
    struct GateProvider {
        caps: ModelCapabilities,
        opened: tokio::sync::watch::Sender<bool>,
        entered: Arc<AtomicUsize>,
    }

    impl Provider for GateProvider {
        fn id(&self) -> &str {
            "gate"
        }

        fn capabilities(&self, _model: &str) -> ModelCapabilities {
            self.caps.clone()
        }

        fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
            use futures::StreamExt;
            self.entered.fetch_add(1, Ordering::SeqCst);
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

    struct GateEnv {
        manager: Arc<SessionManager>,
        agent: Arc<AgentRuntime>,
        orchestrator: Arc<OrchestratorRuntime>,
        executor: Arc<TaskExecutor>,
        parent: SessionId,
        provider: Arc<GateProvider>,
    }

    fn read_caps() -> CapabilitySet {
        CapabilitySet::from_grants([CapabilityGrant::new(
            LatticeCap::ReadWorkspace,
            ScopePattern::new(ScopePattern::WILDCARD).expect("wildcard pattern"),
        )])
        .expect("wildcard grants are sane")
    }

    fn gate_provider() -> Arc<GateProvider> {
        let (opened, _opened_rx) = tokio::sync::watch::channel(false);
        Arc::new(GateProvider {
            caps: ModelCapabilities {
                tools: true,
                parallel_tools: true,
                ..Default::default()
            },
            opened,
            entered: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Build the full daemon-side stack over a store root WITHOUT creating a
    /// session: a reopen of the same root after a "crash" reconstructs the
    /// graph over the SAME durable rows (the only way the in-memory op
    /// registry is truly fresh, exactly like a process restart).
    fn build_gate_stack(
        root: &std::path::Path,
        provider: Arc<GateProvider>,
    ) -> (
        Arc<SessionManager>,
        Arc<AgentRuntime>,
        Arc<OrchestratorRuntime>,
        Arc<TaskExecutor>,
    ) {
        let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let mut registry = ProviderRegistry::new();
        registry.try_register(provider).unwrap();
        let agent = AgentRuntime::new(AgentDeps {
            session: manager.clone(),
            providers: Arc::new(registry),
            chunk_sink: None,
            permission_requester: Arc::new(AlwaysAllow),
            evidence: Arc::new(NoEvidence),
            tools: Arc::new(ToolRegistry::new()),
            cas: None,
            workspaces: faktor_fs::WorkspaceFileService::new(),
            edit: None,
            snapshots: None,
            sandbox: None,
            supervisor: None,
            verification: faktor_agent::VerificationService::disabled(),
            hooks: None,
            instructions_resolver: faktor_instructions::no_roots_resolver(),
            routing: faktor_agent::FixedRoutingPolicy::passthrough(),
            budgets: Arc::new(faktor_session::NoopBudget),
            model: "m".into(),
            compaction_model: None,
            compact_at_usage: 0.65,
            instructions: "You are a test agent.".into(),
            clock: Arc::new(SystemClock),
            tool_call_mode: ToolCallMode::Native,
            tool_deadline_ms: 5000,
            retry_policy: faktor_core::retry::RetryPolicy::default(),
            semantic: faktor_agent::fallback_semantic_registry(),
            context_prior: None,
            secret_registry: None,
            efficiency: Default::default(),
        })
        .unwrap();
        let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
        let executor = TaskExecutor::new_owner_direct_for_test_harness(
            &orchestrator,
            manager.clone(),
            agent.clone(),
        );
        (manager, agent, orchestrator, executor)
    }

    fn open_gate_env(root: &std::path::Path) -> GateEnv {
        let provider = gate_provider();
        let (manager, agent, orchestrator, executor) = build_gate_stack(root, provider.clone());
        let owner_root = root.join("owner");
        std::fs::create_dir_all(&owner_root).unwrap();
        let ws = manager
            .create_workspace(owner_root.to_str().unwrap())
            .unwrap();
        let wt = WorktreeId::new(
            manager
                .put_worktree(ws, owner_root.to_str().unwrap(), "main")
                .unwrap() as u64,
        );
        let parent = manager
            .create_session(ws, "gate-owner", "gate", "m")
            .unwrap()
            .id();
        manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
        GateEnv {
            manager,
            agent,
            orchestrator,
            executor,
            parent,
            provider,
        }
    }

    /// The audited end-to-end invariant: a live detached drive is owned by
    /// the executor; shutdown aborts/awaits it within bound with an empty
    /// registry; the durable task/run rows survive the abort; and the
    /// documented crash-recovery entry (`agent.recover()` +
    /// `agent.continue_turn()`) resumes the SAME recorded turn to a genuine
    /// end — the abort is crash-equivalent, never durable loss.
    #[tokio::test]
    async fn executor_shutdown_aborts_owned_drive_and_crash_recovery_still_resumes() {
        let _heavy = heavy_guard();
        let dir = tempfile::tempdir().unwrap();
        let env = open_gate_env(dir.path());
        let req = TaskRunRequest {
            goal: "parked single-item run".to_string(),
            work_items: vec![WorkItem::new("a", "work a", WorkKind::Analysis)],
            parent_caps: read_caps(),
            ..Default::default()
        };
        let receipt = env
            .executor
            .start_task(env.parent, req)
            .expect("run starts");
        assert_eq!(receipt.mode, TaskRunMode::InSession);
        let run_id = receipt.run_id.clone();
        let parent = env.parent;
        wait_until(|| env.executor.live_drive_count() == 1, 30).await;
        assert_eq!(env.executor.live_drive_runs(), vec![run_id.clone()]);
        // The provider parks INSIDE the model call: the drive cannot finish.
        wait_until(|| env.provider.entered.load(Ordering::SeqCst) >= 1, 30).await;
        let handle = env.manager.get_session(env.parent).unwrap().unwrap();
        let task_id = handle.task_id().unwrap();
        let task_before = handle.get_task(task_id).unwrap().expect("durable task row");
        assert!(!task_before.state.is_terminal());

        let started = Instant::now();
        let report = env
            .executor
            .shutdown_drives(Duration::from_millis(100))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "shutdown drain stayed bounded"
        );
        assert!(report.total >= 1, "the live drive was owned: {report:?}");
        assert!(
            report.aborted >= 1,
            "the parked drive was aborted: {report:?}"
        );
        assert_eq!(report.unreaped, 0, "every drive reaped: {report:?}");
        assert_eq!(env.executor.live_drive_count(), 0);
        assert!(env.executor.drives_shutdown());

        // No durable state loss: the task row and the receipt-named linkage
        // row are still exactly there after the abort.
        let task = handle
            .get_task(task_id)
            .unwrap()
            .expect("task row survives");
        assert!(!task.state.is_terminal());
        assert!(
            handle
                .memory_facts()
                .unwrap()
                .iter()
                .any(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &run_id),
            "the durable run linkage row survives the abort"
        );

        // CRASH: the graph is dropped and REOPENED over the same store —
        // exactly a daemon restart. (The in-process op registry dies with the
        // graph, so the reopened runtime sees a genuine interrupted turn, not
        // a "live in-process driver" fingerprint.)
        let provider = env.provider.clone();
        drop(env);
        let (manager2, agent2, _orchestrator2, _executor2) =
            build_gate_stack(dir.path(), provider.clone());
        let _ = provider.opened.send_replace(true);
        agent2.recover().expect("boot recovery pass");
        let handle2 = manager2.get_session(parent).unwrap().unwrap();
        assert!(
            handle2
                .events_range(0, Some(400))
                .unwrap()
                .iter()
                .any(|e| e.kind == faktor_core::event::EventKind::CrashDetected),
            "the interrupted turn is crash residue"
        );

        // Recovery still resumes: the SAME recorded turn continues to a
        // genuine end once the provider gate opens.
        agent2
            .continue_turn(parent)
            .await
            .expect("the interrupted turn resumes");
        assert!(
            matches!(
                handle2.state().unwrap(),
                faktor_core::state::AgentState::ReadyForNextTurn
                    | faktor_core::state::AgentState::Completed
            ),
            "the resumed turn reached a genuine end"
        );
        assert!(
            handle2
                .get_task(task_id)
                .unwrap()
                .is_some_and(|t| !t.goal.is_empty()),
            "the durable task row is intact after recovery"
        );
    }

    /// Orchestrated counterpart: the run's detached drive is registry-owned;
    /// after shutdown aborts it, the durable plan + assignment rows survive
    /// and a FRESH executor's `resume_run` re-attaches (the recovery entry
    /// point) — the run was never silently dropped.
    #[tokio::test]
    async fn executor_shutdown_keeps_orchestrated_run_resumable() {
        let _heavy = heavy_guard();
        let dir = tempfile::tempdir().unwrap();
        let env = open_gate_env(dir.path());
        let req = TaskRunRequest {
            goal: "two-item parked run".to_string(),
            work_items: vec![
                WorkItem::new("a", "work a", WorkKind::Analysis),
                WorkItem::new("b", "work b", WorkKind::Analysis),
            ],
            parent_caps: read_caps(),
            ..Default::default()
        };
        let receipt = env
            .executor
            .start_task(env.parent, req)
            .expect("run starts");
        assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
        let run_id = receipt.run_id.clone();
        wait_until(|| env.executor.live_drive_count() == 1, 30).await;
        assert_eq!(env.executor.live_drive_runs(), vec![run_id.clone()]);
        // Durable plan + assignment rows land BEFORE the first child spawn:
        // they are the recovery entry point `resume_run` re-attaches to.
        wait_until(
            || {
                !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
                    .unwrap_or_default()
                    .is_empty()
            },
            30,
        )
        .await;
        let report = env
            .executor
            .shutdown_drives(Duration::from_millis(100))
            .await;
        assert!(report.total >= 1, "the live drive was owned: {report:?}");
        assert!(report.aborted >= 1, "{report:?}");
        assert_eq!(report.unreaped, 0, "{report:?}");
        assert_eq!(env.executor.live_drive_count(), 0);
        // The abort cut the drive, NOT the durable markers.
        assert!(
            !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
                .unwrap()
                .is_empty(),
            "durable assignments survive the abort"
        );
        // A fresh executor re-attaches the same durable run.
        let executor2 = TaskExecutor::new_owner_direct_for_test_harness(
            &env.orchestrator,
            env.manager.clone(),
            env.agent.clone(),
        );
        executor2
            .resume_run(env.parent, &run_id, Ceilings::default(), read_caps(), None)
            .expect("re-attach accepted from the surviving durable rows");
        let _ = executor2.shutdown_drives(Duration::from_millis(50)).await;
        assert_eq!(executor2.live_drive_count(), 0);
    }
}

#[cfg(test)]
mod verification_note_tests {
    use super::note_once;
    use std::collections::HashMap;

    #[test]
    fn unavailable_verification_notes_log_once_per_unchanged_reason() {
        let mut notes = HashMap::new();
        assert!(note_once(&mut notes, "tx-1", "no reviewer proof possible"));
        assert!(
            !note_once(&mut notes, "tx-1", "no reviewer proof possible"),
            "an unchanged reason must not re-log"
        );
        assert!(
            note_once(&mut notes, "tx-1", "a different reason"),
            "a changed reason logs again"
        );
        assert!(note_once(&mut notes, "tx-2", "no reviewer proof possible"));
    }
}
