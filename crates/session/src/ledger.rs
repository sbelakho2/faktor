//! The typed durable session ledger (audits 27 / 71-72).
//!
//! The opaque one-row-per-session ledger JSON blob (`task_ledger`) stays as
//! the runtime's working copy; the RICH ledger lives here: a typed,
//! versioned, append-only entry stream (`ledger_entry`) plus a per-session
//! materialized head checkpoint (`ledger_head`) that compaction rewrites.
//!
//! Never-lose contract
//! -------------------
//! Compaction deletes entries ONLY below a durability watermark computed
//! from the fully decoded entry stream, and the watermark NEVER evicts:
//! the LAST `GoalSet`/`CriteriaSet`/`Decision` entry and EVERY unresolved
//! `BlockerOpened` entry (the fold is open-blocks-state, so a resolved
//! opener is deletable, an unresolved one is not). Everything compaction
//! prunes is folded into the head checkpoint first: plan DAG, child-agent
//! records, the routing tail + count, epochs, the last verify run — so
//! compaction is projection-preserving, never FIFO-evicting of durable
//! meaning. The rule is enforced in code and locked by tests.
//!
//! Every payload row carries its own `schema_ver`; decoding is strict:
//! an unknown version or a shape violation is a loud error (corrupt), never
//! a silent parse. A session whose entry stream fails decode FAILS TO OPEN
//! loudly. A head checkpoint that is missing or undecodable is rebuilt by
//! replaying the surviving entries; entries are the authority.
//!
//! Sequence allocation never goes backwards (new seqs stay above the head's
//! checkpoint_seq even after compaction prunes), so "fold entries after the
//! checkpoint" is a correct crash-recovery cursor in both crash orders:
//! appended-then-not-checkpointed and checkpointed-then-crashed.

use std::collections::BTreeMap;

use faktor_core::completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
use serde::{Deserialize, Serialize};

use crate::child::PresentationState;
use crate::handle::SessionHandle;
use crate::task::MAX_VERIFICATION_TREE_HASH_BYTES;
use crate::{json_bytes, map_store_err, SessionError, MAX_LEDGER_BYTES};

// ---------------------------------------------------------------- bounds

/// Payload schema version of every `ledger_entry` row this crate writes.
pub const LEDGER_ENTRY_SCHEMA_V: i64 = 1;
/// Schema version of the `ledger_head` checkpoint JSON.
pub const LEDGER_HEAD_SCHEMA_V: i64 = 1;
/// Max concurrently OPEN blockers a session may hold (bounded everything);
/// opening beyond the cap is a loud error, never a silent drop.
pub const MAX_LEDGER_OPEN_BLOCKERS: usize = 256;
/// Plan-mirror bound: mirrors the durable task row's plan cap
/// (`MAX_TASK_PLAN_STEPS`), so the mirror never exceeds the real plan.
pub const MAX_LEDGER_PLAN_STEPS: u32 = 256;
/// Routing-history tail retained in the materialized head across
/// compactions (the count is exact; the tail is the newest entries).
pub const MAX_LEDGER_ROUTING_TAIL: usize = 128;
/// Bounded page size for typed ledger reads (paging is fundamental).
pub const MAX_LEDGER_PAGE: u64 = 500;
/// Hard bound on ONE entry payload (serialized bytes).
pub const MAX_LEDGER_ENTRY_BYTES: usize = 16 * 1024;
/// Hard bound on one text field inside an entry payload.
pub const MAX_LEDGER_TEXT: usize = 4096;
/// Hard bound on the head checkpoint JSON (the legacy blob's bound).
pub const MAX_LEDGER_HEAD_BYTES: usize = MAX_LEDGER_BYTES;
/// Max criteria rows in one CriteriaSet (the task row's criterion cap).
pub const MAX_LEDGER_CRITERIA: usize = 64;
/// Max child-agent records tracked at once (bounded everything).
pub const MAX_LEDGER_CHILDREN: usize = 256;

// entry_type tags (the explicit schema tag column of every row)
pub const ENTRY_GOAL_SET: &str = "goal_set";
pub const ENTRY_CRITERIA_SET: &str = "criteria_set";
pub const ENTRY_BLOCKER_OPENED: &str = "blocker_opened";
pub const ENTRY_BLOCKER_RESOLVED: &str = "blocker_resolved";
pub const ENTRY_DECISION: &str = "decision";
pub const ENTRY_PLAN_STEP_ADDED: &str = "plan_step_added";
pub const ENTRY_CHILD_AGENT_STARTED: &str = "child_agent_started";
pub const ENTRY_CHILD_AGENT_FINISHED: &str = "child_agent_finished";
pub const ENTRY_ROUTING_DECISION: &str = "routing_decision";
pub const ENTRY_EPOCH_BUMPED: &str = "epoch_bumped";
pub const ENTRY_FAILURE_RECORDED: &str = "failure_recorded";
pub const ENTRY_VERIFY_RUN: &str = "verify_run";
pub const ENTRY_TURN_COMPLETED: &str = "turn_completed";
// Durable learning-crate records (audits 65-67/92): one bounded JSON
// payload per row, stored VERBATIM — the schema inside `payload` is owned
// by `faktor-learning` (an episode, a stored learning, or a removal
// tombstone). Purely additive: no schema migration, no new table; the
// existing `ledger_entry` row stream carries them and compaction pins
// them (learning corpus data is not turn history).
pub const ENTRY_LEARNING_RECORD: &str = "learning_record";
/// The legal `record` tags of one learning row.
pub const LEARNING_RECORD_EPISODE: &str = "episode";
pub const LEARNING_RECORD_LEARNING: &str = "learning";
pub const LEARNING_RECORD_REMOVED: &str = "removed";
/// Hard bound on the opaque JSON payload of one learning record; the
/// surrounding row stays well under [`MAX_LEDGER_ENTRY_BYTES`].
pub const MAX_LEARNING_RECORD_PAYLOAD: usize = 12 * 1024;
// Durable edit-transaction entry kinds (P0-53): the typed rows behind the
// edit engine's record-first multi-file transactions. The OPEN set of a
// session (a `edit_txn_prepared` without a matching terminal) is what crash
// recovery replays; open-txn rows are pinned across compaction.
pub const ENTRY_EDIT_TXN_PREPARED: &str = "edit_txn_prepared";
pub const ENTRY_EDIT_TXN_PROGRESS: &str = "edit_txn_progress";
pub const ENTRY_EDIT_TXN_COMMITTED: &str = "edit_txn_committed";
pub const ENTRY_EDIT_TXN_ROLLED_BACK: &str = "edit_txn_rolled_back";
// Durable multi-candidate tournament rows: the whole lifecycle of one
// implementation tournament (`TournamentStarted` -> zero or more
// `CandidateSettled` -> one `TournamentDecided`). They are the durable
// authority an executor re-opens from after a crash; they fold nowhere in
// the head and are PINNED across compaction (like learning corpus rows) so
// a compacted ledger always reconstructs every tournament exactly.
pub const ENTRY_TOURNAMENT_STARTED: &str = "tournament_started";
pub const ENTRY_CANDIDATE_SETTLED: &str = "candidate_settled";
pub const ENTRY_TOURNAMENT_DECIDED: &str = "tournament_decided";
// Durable foreground/background presentation rows (continuity): one typed
// row per presentation transition of one child. The rows fold into the
// head's presentation map (the LATEST entry per child wins), so compaction
// preserves the child's attention state exactly like every other folded
// projection. Presentation is an ATTENTION concept only — a row never
// changes scheduling ownership, budgets or lineage.
pub const ENTRY_CHILD_PRESENTATION_CHANGED: &str = "child_presentation_changed";
/// Hard bound on the child id of one presentation row.
pub const MAX_PRESENTATION_CHILD_ID: usize = 64;

// Durable terminal-lifecycle rows: the ONE authority behind every terminal
// adapter (native HTTP + ACP). Every transition of one session-owned PTY is a
// typed append-only row — `terminal_created` before the row is exposed,
// `terminal_running` once the child identity is known, then exactly one
// terminal row (`terminal_exited` when the process was observed dead,
// `terminal_killed` when a kill routed through the pty authority, or
// `terminal_lost` when a restart found no live authority for the row). A
// stale lost row is finished by a typed `terminal_reconciled` row
// (`killed`/`collected`). Every row carries the FULL ownership identity
// (session, task, agent, operation, terminal UUID) plus the process identity
// (pid + OS start time), so a restart scan decides reachability from one row
// and NEVER by pid alone. The rows fold nowhere in the head and are PINNED
// across compaction: the durable stream is the authority every adapter
// projects and recovers from.
pub const ENTRY_TERMINAL_CREATED: &str = "terminal_created";
pub const ENTRY_TERMINAL_RUNNING: &str = "terminal_running";
pub const ENTRY_TERMINAL_EXITED: &str = "terminal_exited";
pub const ENTRY_TERMINAL_KILLED: &str = "terminal_killed";
pub const ENTRY_TERMINAL_LOST: &str = "terminal_lost";
pub const ENTRY_TERMINAL_RECONCILED: &str = "terminal_reconciled";

/// Hard bound on one durable terminal UUID (UTF-8 bytes). The daemon mints
/// canonical 36-byte UUID v4 strings; the bound admits every honest id.
pub const MAX_TERMINAL_ID_BYTES: usize = 64;
/// Hard bound on one terminal audit detail (lost reason / kill reason /
/// reconcile disposition note).
pub const MAX_TERMINAL_DETAIL_BYTES: usize = MAX_LEDGER_TEXT;
/// Hard bound on one terminal's effective execution profile (the bounded
/// JSON evidence the execution authority admitted the spawn under).
pub const MAX_TERMINAL_PROFILE_BYTES: usize = 4096;
/// The only legal reconcile disposition tags.
pub const TERMINAL_RECONCILE_KILLED: &str = "killed";
pub const TERMINAL_RECONCILE_COLLECTED: &str = "collected";

// ---------------------------------------------------------------- tournament bounds

/// Hard bound on the candidate band of one tournament (N = 2..=4).
pub const MIN_TOURNAMENT_CANDIDATES: usize = 2;
pub const MAX_TOURNAMENT_CANDIDATES: usize = 4;
/// Max criteria rows of one tournament.
pub const MAX_TOURNAMENT_CRITERIA: usize = 16;
/// Max bytes of one criterion id / spec / candidate text field.
pub const MAX_TOURNAMENT_TEXT: usize = 512;
/// Max bytes of one tournament id / run family / child id.
pub const MAX_TOURNAMENT_ID: usize = 64;
/// Max bytes of the decision outcome text.
pub const MAX_TOURNAMENT_OUTCOME: usize = 1024;
/// The only legal settlement state tags.
pub const TOURNAMENT_STATE_DONE: &str = "done";
pub const TOURNAMENT_STATE_FAILED: &str = "failed";
pub const TOURNAMENT_STATE_CANCELLED: &str = "cancelled";
/// The only legal independent-review verdict ranks (ascending severity).
pub const TOURNAMENT_REVIEW_BLOCK: &str = "block";
pub const TOURNAMENT_REVIEW_CONCERN: &str = "concern";
pub const TOURNAMENT_REVIEW_CLEAN: &str = "clean";
/// The only legal tournament outcome tags.
pub const TOURNAMENT_OUTCOME_DECIDED: &str = "decided";
pub const TOURNAMENT_OUTCOME_ABORTED: &str = "aborted";

// Durable agent-coordination board rows: the parent/descendant-scoped
// coordination board of ONE run family lives in the ROOT session's typed
// ledger as append-only rows (`board_post` / `board_read` /
// `board_receipt` / `board_reset`). All board rows are PINNED across
// compaction (like learning corpus rows): the pinned stream is the durable
// authority a board read reconstructs the live surface from after a crash
// or reopen. The root session's ledger head caches the board revision for
// O(1) CAS allocation; the entry stream remains the authority.
pub const ENTRY_BOARD_POST: &str = "board_post";
pub const ENTRY_BOARD_READ: &str = "board_read";
pub const ENTRY_BOARD_RECEIPT: &str = "board_receipt";
pub const ENTRY_BOARD_RESET: &str = "board_reset";

// ---------------------------------------------------------------- board bounds

/// Hard bound on one board post subject (UTF-8 bytes).
pub const MAX_BOARD_SUBJECT_BYTES: usize = 512;
/// Hard bound on one board post body (16 KiB, as advertised). The
/// serialized row must additionally fit [`MAX_LEDGER_ENTRY_BYTES`], so a
/// body near this bound is only accepted when its JSON encoding fits the
/// ledger entry cap (an oversized encoded row is a typed reject).
pub const MAX_BOARD_BODY_BYTES: usize = 16 * 1024;
/// Max evidence/path/artifact references of one board post.
pub const MAX_BOARD_REFS: usize = 32;
/// Max bytes of one board post reference.
pub const MAX_BOARD_REF_BYTES: usize = 1024;
/// Max bytes of one board receipt note.
pub const MAX_BOARD_RECEIPT_NOTE_BYTES: usize = 2048;
/// The only legal board receipt action tags.
pub const BOARD_RECEIPT_ACK: &str = "ack";
pub const BOARD_RECEIPT_TASK_UPDATE: &str = "task_update";
pub const BOARD_RECEIPT_BLOCKED: &str = "blocked";
pub const BOARD_RECEIPT_QUESTION: &str = "question";
/// Bounded page size for one descending board scan step.
pub const MAX_BOARD_SCAN_PAGE: u64 = 500;
/// Hard cap on receipts returned for one `(child, post)` pair; beyond this
/// the read refuses loudly instead of returning an unbounded list.
pub const MAX_BOARD_RECEIPTS_PER_POST: usize = 256;

// Durable PR/CI-fix completion-contract rows (P2): `completion_contract_set`
// records the accepted contract of ONE task run (keyed by the task revision
// the run started at; immutable per revision) and `completion_step_status`
// records one per-step outcome (`succeeded|failed|skipped`) with a bounded
// detail. Both rows are the durable authority the `VerifiedComplete` gate
// reads; they fold nowhere in the head and are PINNED across compaction so a
// compacted ledger can always re-evaluate the gate exactly.
pub const ENTRY_COMPLETION_CONTRACT_SET: &str = "completion_contract_set";
pub const ENTRY_COMPLETION_STEP_STATUS: &str = "completion_step_status";

// Durable integration records (P0 orchestrated-completion binding): one
// `integration_record` row per integration attempt of an orchestrated run.
// It binds the run's root verification (and every completion-contract step
// status) to the ACTUAL final integration root snapshot, so an unrelated
// owner-checkout edit invalidates the verification instead of silently
// satisfying completion. Integration rows are the durable authority a
// restarted executor re-opens from; they fold nowhere in the head and are
// PINNED across compaction (a pruned record would silently unbind a passing
// verification record from the root it certified).
pub const ENTRY_INTEGRATION_RECORD: &str = "integration_record";

// ---------------------------------------------------------------- completion bounds

/// Hard bound on the bounded detail text of one step-status row.
pub const MAX_COMPLETION_STEP_DETAIL: usize = MAX_LEDGER_TEXT;

// ---------------------------------------------------------------- integration bounds

/// Hard bound on the number of source rows of one integration record (one
/// row per integrated isolated child). A run beyond this bound records the
/// deterministic prefix plus `source_count`/`sources_digest` in the record
/// (explicit, never a silent omission).
pub const MAX_INTEGRATION_SOURCES: usize = 32;
/// Hard bound on the stored integrated-file path sample (the FULL list is
/// covered by `integrated_files_digest` + `integrated_file_count`).
pub const MAX_INTEGRATION_FILES: usize = 16;
/// Hard bound on one stored integrated-file path.
pub const MAX_INTEGRATION_PATH_BYTES: usize = 256;
/// Hard bound on the stored conflict summaries (the FULL count rides
/// `conflict_count`).
pub const MAX_INTEGRATION_CONFLICTS: usize = 8;
/// Hard bound on one stored conflict summary line.
pub const MAX_INTEGRATION_CONFLICT_BYTES: usize = 256;
/// Hard bound on the integration root path.
pub const MAX_INTEGRATION_ROOT_BYTES: usize = 1024;
/// Hard bound on a run id / change-set id / child id inside the record.
pub const MAX_INTEGRATION_ID_BYTES: usize = 128;
/// The entry tag of one integration record (mirrors the enum tag).
pub const INTEGRATION_RECORDED_TAG: &str = "integration_recorded";

// Durable orchestrated run-base + landing-transaction rows (prepare ->
// verify(candidate) -> land(owner)): one `run_base` row records the
// IMMUTABLE run-base generation every isolated child is derived from, and
// one `integration_txn` row per landing attempt journals every per-path
// decision, its rollback blob and the transaction phase. Both fold nowhere
// in the head and are PINNED across compaction: the run base is the CAS
// anchor staging refuses against, and an unfinished landing transaction is
// the durable recovery authority a restarted executor finishes or rolls
// back from.
pub const ENTRY_RUN_BASE: &str = "run_base";
pub const ENTRY_INTEGRATION_TXN: &str = "integration_txn";

// Durable external-operation identity rows (fail-closed external effects):
// one `external_operation` row per attempt of one provider-neutral external
// operation (the native PR step today). The row is written BEFORE the remote
// call and records the exact input identity the provider lookup is keyed by
// (organization/repository + exact head/base + a stable Faktor task marker);
// after the call an updated row records the remote object id + version. A
// crash mid-operation therefore reconciles from the RECORDED identity instead
// of guessing, a second operation under the same key with a different input
// identity is a typed refusal, and no reconciliation path can ever mint a
// duplicate remote object. The rows fold nowhere in the head and are PINNED
// across compaction: the reconciliation authority must outlive any watermark.
pub const ENTRY_EXTERNAL_OPERATION: &str = "external_operation";
/// The durable VERIFIED-GIT publication artifact of a completion contract
/// revision: the exact verified manifest + tree/commit/remote OIDs the
/// commit/push/PR steps must publish (and re-assert before every side
/// effect). Pinned across compaction.
pub const ENTRY_VERIFIED_GIT_ARTIFACT: &str = "verified_git_artifact";

// ---------------------------------------------------------------- external-operation bounds

/// Hard bound on one external-operation id / operation key / provider /
/// kind / remote object id / remote object version / input marker.
pub const MAX_EXTERNAL_OPERATION_TEXT_BYTES: usize = 512;
/// Hard bound on one organization/repository/head/base reference.
pub const MAX_EXTERNAL_OPERATION_REF_BYTES: usize = 1024;

// ---------------------------------------------------------------- run-base / txn bounds

/// Hard bound on the stored run-base path.
pub const MAX_RUN_BASE_ROOT_BYTES: usize = 1024;
/// Hard bound on the stored manifest digest / snapshot hashes.
pub const MAX_RUN_BASE_DIGEST_BYTES: usize = 64;
/// Hard bound on the number of per-path rows of one landing transaction
/// (mirrors the change-set file cap; the transaction never silently drops a
/// decision).
pub const MAX_INTEGRATION_TXN_PATHS: usize = 2000;
/// Hard bound on the conflict summaries of one landing transaction.
pub const MAX_INTEGRATION_TXN_CONFLICTS: usize = 32;
/// Hard bound on one conflict summary line.
pub const MAX_INTEGRATION_TXN_CONFLICT_BYTES: usize = 256;

// ---------------------------------------------------------------- edit txn bounds

/// Max files in ONE edit transaction (mirrors the engine's bound).
pub const MAX_EDIT_TXN_FILES: usize = 2000;
/// Max bytes of one edit-transaction `session` label.
pub const MAX_EDIT_TXN_SESSION_BYTES: usize = 128;
/// Max path rows one terminal (`committed`/`rolled_back`) payload may list.
pub const MAX_EDIT_TXN_TERMINAL_PATHS: usize = MAX_EDIT_TXN_FILES;
/// The only legal strategy tags of an edit transaction.
pub const EDIT_TXN_STRATEGY_ROLL_FORWARD: &str = "roll_forward";
pub const EDIT_TXN_STRATEGY_ROLL_BACK: &str = "roll_back";
/// The only legal per-file outcome tags of an edit transaction.
pub const EDIT_TXN_OUTCOME_COMMITTED: &str = "committed";
pub const EDIT_TXN_OUTCOME_CONFLICTED: &str = "conflicted";

// ---------------------------------------------------------------- typed payloads

/// One `(check id, passed)` row of a VerifyRun.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerCheckRun {
    pub id: String,
    pub passed: bool,
}

/// The durable identity + ownership of ONE session-owned terminal. Every
/// terminal lifecycle row carries this complete record, so a restart scan
/// never needs a second store: `terminal_id` is the authority key (UUID),
/// `session_id`/`task_id`/`agent_id`/`operation_id` the ownership, and
/// `pid`+`start_time_ms` the process identity (an OS start time of 0 means
/// the process was never observed — such a row is NEVER adopted by pid
/// alone). `execution_profile` is the EFFECTIVE profile the execution
/// authority admitted the spawn under (candidate root, cwd, capabilities,
/// filesystem/network projections, budgets) — the row records the profile,
/// not merely the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalDurableRow {
    pub terminal_id: String,
    pub session_id: u64,
    pub task_id: u64,
    #[serde(default)]
    pub agent_id: Option<String>,
    pub operation_id: u64,
    /// The child pid at spawn (0 = never observed).
    pub pid: u32,
    /// OS-reported process start-time marker in milliseconds (epoch-based
    /// where the platform reports epoch time; since-boot ticks on Linux),
    /// 0 = unknown. Recovery compares it for equality only — a marker is
    /// never interpreted as a signal target.
    pub start_time_ms: i64,
    /// The journal time of THIS row (ms since the Unix epoch).
    pub at_ms: i64,
    /// The effective execution profile the spawn was authorized under,
    /// bounded JSON evidence. Empty on rows written before the execution
    /// authority existed (legacy rows stay readable; new spawns always
    /// record the profile).
    #[serde(default)]
    pub execution_profile: String,
}

/// The six durable terminal lifecycle kinds, in the order a healthy terminal
/// walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalEventKind {
    Created,
    Running,
    Exited,
    Killed,
    Lost,
    Reconciled,
}

impl TerminalEventKind {
    /// The durable `entry_type` column tag of this kind.
    pub fn as_tag(self) -> &'static str {
        match self {
            TerminalEventKind::Created => ENTRY_TERMINAL_CREATED,
            TerminalEventKind::Running => ENTRY_TERMINAL_RUNNING,
            TerminalEventKind::Exited => ENTRY_TERMINAL_EXITED,
            TerminalEventKind::Killed => ENTRY_TERMINAL_KILLED,
            TerminalEventKind::Lost => ENTRY_TERMINAL_LOST,
            TerminalEventKind::Reconciled => ENTRY_TERMINAL_RECONCILED,
        }
    }

    /// The short lifecycle state tag (`created|running|exited|killed|lost|
    /// reconciled`) every projection reports.
    pub fn state_tag(self) -> &'static str {
        match self {
            TerminalEventKind::Created => "created",
            TerminalEventKind::Running => "running",
            TerminalEventKind::Exited => "exited",
            TerminalEventKind::Killed => "killed",
            TerminalEventKind::Lost => "lost",
            TerminalEventKind::Reconciled => "reconciled",
        }
    }

    pub fn parse(tag: &str) -> Option<Self> {
        match tag {
            ENTRY_TERMINAL_CREATED => Some(TerminalEventKind::Created),
            ENTRY_TERMINAL_RUNNING => Some(TerminalEventKind::Running),
            ENTRY_TERMINAL_EXITED => Some(TerminalEventKind::Exited),
            ENTRY_TERMINAL_KILLED => Some(TerminalEventKind::Killed),
            ENTRY_TERMINAL_LOST => Some(TerminalEventKind::Lost),
            ENTRY_TERMINAL_RECONCILED => Some(TerminalEventKind::Reconciled),
            _ => None,
        }
    }

    /// Whether the kind is terminal (no further transition except a typed
    /// reconcile of a Lost row).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TerminalEventKind::Exited
                | TerminalEventKind::Killed
                | TerminalEventKind::Lost
                | TerminalEventKind::Reconciled
        )
    }
}

/// One decoded durable terminal row (read surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalLedgerRecord {
    pub seq: i64,
    pub kind: TerminalEventKind,
    pub row: TerminalDurableRow,
    /// The observed exit code (Exited rows only; `None` = unknown).
    pub exit_code: Option<i32>,
    /// The bounded audit detail of the row (kill/lost reason, reconcile
    /// disposition). Empty for Created/Running/Exited.
    pub detail: String,
}

/// One staged file of a durable edit transaction (`edit_txn_prepared`).
/// `base_digest` is the lowercase 64-hex BLAKE3 of the content the
/// transaction validated against; `base_bytes_len` its length.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditTxnLedgerFile {
    pub path: String,
    pub base_digest: String,
    pub base_bytes_len: u64,
}

/// One decoded progress row of an open edit transaction (read surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTxnOpenProgress {
    pub seq: u64,
    pub path: String,
    pub outcome: String,
}

/// One OPEN (prepared without a terminal) edit transaction (read surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditTxnOpenRow {
    pub txn_id: u64,
    pub session: String,
    pub strategy: String,
    pub files: Vec<EditTxnLedgerFile>,
    pub progress: Vec<EditTxnOpenProgress>,
}

/// One criterion of a tournament: its deterministic id plus the exact
/// specification text every candidate was handed (byte-identical across
/// candidates is an executor-side assertion; these rows are the durable
/// copy).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentCriterionRow {
    pub id: String,
    pub spec: String,
}

/// One derived verification check of a tournament criterion. The check SET
/// is a pure function of the criteria; every candidate settlement must
/// carry the byte-identical set (the orchestrator asserts it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentCheckSpec {
    pub id: String,
    pub spec: String,
}

/// One candidate seed of a tournament: the child it names and where that
/// child's isolated work landed at start time (`worktree`/`base_revision`
/// may be empty until the first settlement records the real location).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentCandidateRow {
    pub child_id: String,
    pub worktree: String,
    pub base_revision: String,
}

/// One settled candidate: the full evidence the deterministic comparison
/// consumes. `state` is `done|failed|cancelled`; `verification` is the
/// durable verification record id (raw, optional), `checks` the derived
/// check set the candidate was verified against, `review` the independent
/// reviewer's rank (`block|concern|clean`) with its reviewer identity, and
/// `reason` the bounded audit text (why settled/discarded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TournamentSettlementRow {
    pub child_id: String,
    pub worktree: String,
    pub base_revision: String,
    pub state: String,
    /// Durable verification record id (raw u64); `None` = no record.
    #[serde(default)]
    pub verification: Option<u64>,
    /// The verification verdict the record certified; `None` = unverified.
    #[serde(default)]
    pub verification_pass: Option<bool>,
    pub checks: Vec<TournamentCheckSpec>,
    /// `block|concern|clean`; `None` = not reviewed (never eligible).
    #[serde(default)]
    pub review: Option<String>,
    /// The independent reviewer's identity (child id / agent id); must
    /// differ from every candidate child id.
    #[serde(default)]
    pub reviewer: Option<String>,
    #[serde(default)]
    pub cost_micro: u64,
    #[serde(default)]
    pub wall_ms: u64,
    #[serde(default)]
    pub reason: String,
}

/// The typed payload of ONE ledger entry. The serde `kind` field is the
/// per-payload schema tag inside the row JSON (schema v1); the row's
/// `entry_type` column is the same tag, stored redundantly so the stream
/// is decodable without touching the payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum LedgerPayload {
    /// The task goal (last wins). `{goal}`.
    GoalSet { goal: String },
    /// The wave-8 acceptance criteria as derived: full row list plus the
    /// canonical joined text (last wins). `{criteria, canonical}`.
    CriteriaSet {
        criteria: Vec<String>,
        canonical: String,
    },
    /// A verification/completion gate opened with this reason.
    /// `{reason}`. Unresolved openers are never evicted.
    BlockerOpened { reason: String },
    /// The blocker with this reason is cleared. `{reason}`.
    BlockerResolved { reason: String },
    /// One decision at a decision point. `{step, choice, rationale}`.
    Decision {
        step: String,
        choice: String,
        rationale: String,
    },
    /// One plan DAG node; `parent_index` links to the prior step it extends
    /// (None for the first step). Mirror of the typed task row's plan.
    PlanStepAdded {
        step_index: u32,
        text: String,
        parent_index: Option<u32>,
    },
    /// A child agent started. `{agent_id, task_id, worktree_id, purpose}`.
    ChildAgentStarted {
        agent_id: u64,
        task_id: u64,
        worktree_id: u64,
        purpose: String,
    },
    /// A child agent finished. `{agent_id, outcome}`. Append-time typed
    /// error when no matching running child exists.
    ChildAgentFinished { agent_id: u64, outcome: String },
    /// One economic routing decision of one turn. `{turn, provider, model,
    /// reasoning, cost_micro}`.
    RoutingDecision {
        turn: u64,
        provider: String,
        model: String,
        reasoning: String,
        cost_micro: u64,
    },
    /// Instruction epoch changed (rule files changed across a reload).
    /// `{from, to}`; `from` is None when no epoch was recorded yet.
    EpochBumped { from: Option<u64>, to: u64 },
    /// One recorded failure of a turn.
    FailureRecorded { failure: String },
    /// One genuine end-of-turn verification run: every check that ran with
    /// its pass/fail plus the outcome tag
    /// (`passed|failed|blocked|pending|unverified`).
    VerifyRun {
        checks: Vec<LedgerCheckRun>,
        outcome: String,
    },
    /// One genuine logical-turn completion. `{turn}` (op-based turn id).
    TurnCompleted { turn: u64 },
    /// One durable learning-crate record (audits 65-67/92): `payload` is
    /// the learning crate's bounded JSON document stored verbatim, and
    /// `record` is its kind (`episode` | `learning` | `removed`). The
    /// session ledger never interprets the payload; the learning crate
    /// strictly decodes it on read (a corrupt payload is loud there).
    LearningRecord { record: String, payload: String },
    /// A durable multi-file edit transaction was prepared (P0-53): every
    /// file staged and validated, nothing written yet. `{txn_id, session,
    /// files, strategy}`; `strategy` is `roll_forward` | `roll_back`.
    EditTxnPrepared {
        txn_id: u64,
        session: String,
        files: Vec<EditTxnLedgerFile>,
        strategy: String,
    },
    /// One per-file outcome of an open edit transaction, journaled AFTER the
    /// file's commit-time CAS. `{txn_id, seq, path, outcome}` where `seq` is
    /// the file's index in the prepared list and `outcome` is `committed` |
    /// `conflicted`. Journaled after each write: a crash between a write and
    /// its progress row is detected by recovery as an ambiguous file that is
    /// never silently accepted or clobbered.
    EditTxnProgress {
        txn_id: u64,
        seq: u64,
        path: String,
        outcome: String,
    },
    /// The terminal row of a transaction that finished WITHOUT a rollback:
    /// every file committed (or conflicted, for `roll_forward` conflict
    /// stops). `{txn_id, committed, conflicted, skipped}` are the paths.
    EditTxnCommitted {
        txn_id: u64,
        committed: Vec<String>,
        conflicted: Vec<String>,
        skipped: Vec<String>,
    },
    /// The terminal row of a `roll_back` transaction that hit a conflict:
    /// the already-committed files were CAS-restored to their staged
    /// before-content. `{txn_id, rolled_back, rollback_conflicts}`; a path
    /// in `rollback_conflicts` changed again after our write and was NEVER
    /// clobbered.
    EditTxnRolledBack {
        txn_id: u64,
        rolled_back: Vec<String>,
        rollback_conflicts: Vec<String>,
    },
    /// One implementation tournament was started: the goal every candidate
    /// received, the byte-identical criteria, and the N candidate seeds
    /// (`child-0..child-{N-1}`) with their isolated worktrees. The durable
    /// anchor a crash re-opens from.
    TournamentStarted {
        tournament_id: String,
        run_family: String,
        goal: String,
        criteria: Vec<TournamentCriterionRow>,
        candidates: Vec<TournamentCandidateRow>,
    },
    /// One candidate settled: its child terminal state, location, and the
    /// verification + independent-review evidence the comparison ranks.
    CandidateSettled {
        tournament_id: String,
        settlement: TournamentSettlementRow,
    },
    /// The tournament reached a terminal state: `outcome` is
    /// `decided|aborted`; `winner` names the candidate proposed for the
    /// explicit approved-merge path (None on abort). `rationale` records
    /// WHY (bounded), including the loser-discard reason.
    TournamentDecided {
        tournament_id: String,
        winner: Option<String>,
        outcome: String,
        rationale: String,
    },
    /// One durable child presentation transition (foreground <-> background
    /// continuity): the child it belongs to, the from/to states and the
    /// transition time. `from == to` is refused (a no-op carries no row) and
    /// the rows fold into the head's presentation map, latest entry per
    /// child. Purely an attention/presentation record: it never changes the
    /// child's scheduling ownership, budgets or lineage.
    ChildPresentationChanged {
        child_id: String,
        from: PresentationState,
        to: PresentationState,
        at_ms: i64,
    },
    /// One coordination-board post. `board_id` is the ROOT session's raw
    /// id (one board per run family); `post_id` is the post's durable
    /// identity and equals `revision` (revisions are never reused, a reset
    /// consumes one); `author_child` is the acting child's session raw id
    /// (None = the root agent); `author_session` the session whose handle
    /// posted; `revision` the per-board monotonic revision. `body` is
    /// bounded by [`MAX_BOARD_BODY_BYTES`] and `refs` by [`MAX_BOARD_REFS`].
    BoardPost {
        board_id: u64,
        post_id: u64,
        author_child: Option<u64>,
        author_session: u64,
        subject: String,
        body: String,
        refs: Vec<String>,
        revision: u64,
    },
    /// One read receipt: `child` read `post_id`. Read rows are append-only
    /// and idempotent per (child, post) — a re-read never duplicates a row.
    BoardRead {
        board_id: u64,
        child: u64,
        post_id: u64,
    },
    /// One action receipt on a board post (`ack` / `task_update` /
    /// `blocked` / `question`); `child` is None for a root-agent action.
    BoardReceipt {
        board_id: u64,
        child: Option<u64>,
        post_id: u64,
        action: String,
        note: String,
    },
    /// The durable reset marker: the live reading surface restarts AFTER
    /// this row (`previous_revision` CAS-checked, `new_revision` bumped by
    /// exactly one). History before the marker is retained in the stream
    /// but hidden from reads.
    BoardReset {
        board_id: u64,
        previous_revision: u64,
        new_revision: u64,
    },
    /// The accepted PR/CI-fix completion contract of ONE task run (P2):
    /// `task_id` + the task `revision` the run started at, plus the
    /// requested steps. A contract is immutable per (task, revision) — a
    /// second set for the same revision is a typed Conflict. All-false is
    /// never recorded (it is the default behavior and carries no row).
    CompletionContractSet {
        task_id: u64,
        revision: u64,
        contract: CompletionContract,
    },
    /// One durable outcome of ONE requested completion step: `step` is
    /// `commit|push|pr`, `status` is `succeeded|failed|skipped`, `detail`
    /// is bounded audit text, `at_ms` the record time. The completion gate
    /// requires a `Succeeded` row for every requested step; a `Failed` row
    /// is a terminal refusal.
    CompletionStepStatus {
        task_id: u64,
        revision: u64,
        step: CompletionStep,
        status: CompletionStepOutcome,
        detail: String,
        /// The final integration snapshot the outcome was recorded against.
        /// Additive with a serde default: rows written before the
        /// integration binding decode with `None` (the legacy behavior).
        #[serde(default)]
        snapshot: Option<String>,
        at_ms: i64,
    },
    /// One durable integration record of an orchestrated run (P0
    /// orchestrated-completion binding): the staged sources, the final
    /// integration root and its snapshot digest, the integrated files and
    /// any conflicts. Record-first and pinned across compaction.
    IntegrationRecorded { record: IntegrationRecordRow },
    /// The durable IMMUTABLE run base of one orchestrated run, recorded
    /// BEFORE the first child spawn. Pinned across compaction.
    RunBaseRecorded { record: RunBaseRecord },
    /// One durable verified-git publication artifact of a completion
    /// contract revision (exact manifest + tree/commit/remote OIDs).
    /// Pinned across compaction: the completion steps re-assert it.
    VerifiedGitArtifactRecorded { artifact: VerifiedGitArtifact },
    /// One durable landing transaction of an orchestrated run: record-first
    /// per-path decisions + rollback blobs, then the deterministic phase a
    /// crashed executor finishes landing or rolls back from. Pinned across
    /// compaction.
    IntegrationTxnRecorded { row: IntegrationTxnRow },
    /// One durable external-operation row (write-before-call identity then
    /// the reconciliation outcome). Pinned across compaction: the exact
    /// recorded input identity is the only authority a restart may
    /// reconcile an unfinished external effect from.
    ExternalOperationRecorded { record: ExternalOperationRow },
    /// One session-owned terminal was recorded BEFORE its row was exposed:
    /// the full ownership + process identity. A crash between this row and
    /// `TerminalRunning` leaves a row no live authority owns, which recovery
    /// marks Lost.
    TerminalCreated { row: TerminalDurableRow },
    /// The terminal's child is up and its process identity is known.
    TerminalRunning { row: TerminalDurableRow },
    /// The child process was observed dead (swept by the authority); the
    /// exit code is `None` when the pty backend cannot report one.
    TerminalExited {
        row: TerminalDurableRow,
        #[serde(default)]
        exit_code: Option<i32>,
    },
    /// A kill routed through the pty authority terminated the tree.
    TerminalKilled {
        row: TerminalDurableRow,
        reason: String,
    },
    /// A restart (or an unreachable row) found no live authority for the
    /// row; the process identity decides whether the recorded process is
    /// gone or its pid was recycled (`reason` is the bounded audit text).
    TerminalLost {
        row: TerminalDurableRow,
        reason: String,
    },
    /// A stale Lost row was finished exactly once, typed: `disposition` is
    /// `killed` (the row's process was proven dead/recycled and the row was
    /// closed as killed) or `collected` (the row was retired as unreachable).
    TerminalReconciled {
        row: TerminalDurableRow,
        disposition: String,
    },
}

/// Decode one durable ledger row strictly: an unknown kind, an unknown
/// schema version or a shape violation is a loud `Malformed`, never a silent
/// drop. Additive public seam (the retention reference scanner walks durable
/// rows without a live [`SessionHandle`]); `SessionHandle::decode_row`
/// delegates here so both paths share ONE decoder.
pub fn decode_ledger_entry_row(
    row: &faktor_store::LedgerEntryRow,
) -> Result<TypedLedgerEntry, SessionError> {
    let payload = decode_payload(&row.entry_type, row.schema_ver, &row.payload)?;
    Ok(TypedLedgerEntry {
        seq: row.seq,
        entry_type: row.entry_type.clone(),
        schema_ver: row.schema_ver,
        payload,
        created_ms: row.created_ms,
    })
}

/// One decoded ledger row.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedLedgerEntry {
    pub seq: i64,
    pub entry_type: String,
    pub schema_ver: i64,
    pub payload: LedgerPayload,
    pub created_ms: i64,
}

/// The typed fold projection of the ledger: what compaction prunes is
/// folded HERE before it may delete entries.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerHead {
    /// Head JSON schema version ([`LEDGER_HEAD_SCHEMA_V`]).
    pub schema_ver: i64,
    pub goal: String,
    pub criteria: Vec<String>,
    pub canonical_criteria: String,
    pub open_blockers: Vec<String>,
    /// Decisions newest-first, bounded (the last Decision entry is also
    /// pinned in the stream, so the head's oldest entries age out only
    /// below the pinned newest).
    pub decisions: Vec<LedgerDecision>,
    /// Plan DAG: one row per step_index, ascending.
    pub plan_steps: Vec<LedgerPlanStep>,
    /// Child-agent records (running + finished), keyed by agent_id.
    pub children: Vec<LedgerChild>,
    /// Exact routing-decision count; `routing_tail` is the bounded newest
    /// tail that survives compaction.
    pub routing_count: u64,
    pub routing_tail: Vec<LedgerRouting>,
    /// The current instruction epoch (last EpochBumped target).
    pub epoch: Option<u64>,
    /// The last genuine VerifyRun (bounded summary).
    pub last_verify: Option<LedgerVerifySummary>,
    /// Newest coordination-board revision folded so far (0 = none). The
    /// board rows themselves are pinned in the stream; this head field is
    /// only the O(1) revision allocation/CAS cursor and is rebuilt by
    /// replay when a head is missing. Additive with a serde default so
    /// heads written before the board feature decode unchanged.
    #[serde(default)]
    pub board_revision: u64,
    /// The durable presentation/attention state of every child that ever
    /// transitioned (the LATEST `ChildPresentationChanged` entry per child
    /// id wins; a child absent from the map is Foreground). Additive with a
    /// serde default so heads written before presentation continuity decode
    /// unchanged; the entry stream remains the authority and the map is
    /// rebuilt by replay when a head is missing.
    #[serde(default)]
    pub presentations: BTreeMap<String, PresentationState>,
    /// The materialized checkpoint: seq of the newest folded entry (0 when
    /// nothing is folded yet). Compaction rewrites it to the pre-prune max;
    /// appends always allocate ABOVE it, so the fold cursor never rewinds.
    pub checkpoint_seq: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerDecision {
    pub step: String,
    pub choice: String,
    pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerPlanStep {
    pub step_index: u32,
    pub text: String,
    pub parent_index: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerChild {
    pub agent_id: u64,
    pub task_id: u64,
    pub worktree_id: u64,
    pub purpose: String,
    /// Some when the child finished; the entry stream records the moment.
    pub outcome: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerRouting {
    pub turn: u64,
    pub provider: String,
    pub model: String,
    pub reasoning: String,
    pub cost_micro: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LedgerVerifySummary {
    pub checks: Vec<LedgerCheckRun>,
    pub outcome: String,
}

/// A typed, paged read of the entry stream (bounded).
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerEntryPage {
    pub entries: Vec<TypedLedgerEntry>,
    pub has_more: bool,
}

/// One decoded durable learning-crate record: the session-side read surface
/// consumed by `faktor-learning`'s durable store adapter (`seq` is the
/// record's durable order; `record` its kind; `payload` the learning JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LearningRecordRow {
    pub seq: i64,
    pub record: String,
    pub payload: String,
}

/// One accepted completion contract read from the durable stream: the run's
/// `(task_id, revision)` identity plus the requested steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionContractRow {
    pub seq: i64,
    pub task_id: u64,
    pub revision: u64,
    pub contract: CompletionContract,
}

/// One durable per-step outcome read from the stream, ascending by seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionStepStatusRow {
    pub seq: i64,
    pub task_id: u64,
    pub revision: u64,
    pub step: CompletionStep,
    pub status: CompletionStepOutcome,
    pub detail: String,
    /// The final integration snapshot the step outcome was recorded against
    /// (`None` for runs without an integration record, e.g. single-session
    /// drives — the legacy behavior, byte-identical). A step recorded with a
    /// snapshot does not satisfy the completion gate once the root snapshot
    /// moved away from it.
    pub snapshot: Option<String>,
    pub at_ms: i64,
}

/// The explicit outcome of one durable read (mandate: missing vs corrupt).
/// The four cases are NEVER collapsed: every consumer matches them and
/// applies its own policy.
///
/// - [`Self::Missing`]: the row genuinely does not exist — the caller
///   applies its migration/not-found policy;
/// - [`Self::PresentValid`]: present and strictly decoded — the caller uses
///   it;
/// - [`Self::PresentMalformed`]: present but undecodable or shape-invalid —
///   corruption, refused with a typed `CorruptDurableState`, never "absent";
/// - [`Self::StoreFailure`]: the durable read itself failed (store/I/O) — an
///   error, never "nothing happened".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableRead<T> {
    Missing,
    PresentValid(T),
    PresentMalformed(String),
    StoreFailure(String),
}

impl<T> DurableRead<T> {
    /// Classify one session-read failure: a strict decode/shape refusal is
    /// corruption (`PresentMalformed`); everything else is a store failure.
    pub fn from_session_error(e: SessionError) -> Self {
        match e {
            SessionError::Malformed(detail) => Self::PresentMalformed(detail),
            other => Self::StoreFailure(other.to_string()),
        }
    }

    /// The row genuinely does not exist.
    pub fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }

    /// Present and strictly decoded.
    pub fn valid_ref(&self) -> Option<&T> {
        match self {
            Self::PresentValid(value) => Some(value),
            _ => None,
        }
    }

    /// Present and strictly decoded (owned).
    pub fn valid(self) -> Option<T> {
        match self {
            Self::PresentValid(value) => Some(value),
            _ => None,
        }
    }

    /// Present but corrupt.
    pub fn is_present_malformed(&self) -> bool {
        matches!(self, Self::PresentMalformed(_))
    }
}

/// One integrated isolated child of an [`IntegrationRecordRow`]: the child,
/// the staged change-set it contributed and the content digest of its
/// candidate root at integration time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSourceRow {
    pub child_id: String,
    pub change_set_id: String,
    /// Lowercase 64-hex BLAKE3 digest of the child's candidate root.
    pub candidate_root_hash: String,
}

/// The durable integration record of ONE orchestrated run (P0
/// orchestrated-completion binding). Written record-first (an in-flight row
/// with an empty `final_snapshot_hash` before any file apply, finalized
/// after every apply) and pinned across compaction. The root verification
/// record and every completion step status of the run reference
/// `final_snapshot_hash`; completion re-derives the root snapshot and
/// refuses a record whose snapshot no longer matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationRecordRow {
    pub run_id: String,
    /// The root task row the integration serves.
    pub task_id: u64,
    /// DEPRECATED (hardening): the old overloaded "base revision", derived
    /// from `sources.first()`, mixed a change-set id with a revision. It is
    /// never populated by the orchestrator anymore; the explicit snapshot
    /// fields below carry the real run base / candidate / landed identity.
    /// Decode stays lenient because pre-hardening rows and the (out-of-scope)
    /// native server rows may still carry it; readers must prefer the
    /// explicit fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    /// DEPRECATED alias of [`Self::run_base_snapshot`]: the base root digest
    /// before any child apply. New rows populate both identically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot: Option<String>,
    /// The IMMUTABLE run base snapshot digest (the generation every staged
    /// change set derived from). Explicit field; never derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_base_snapshot: Option<String>,
    /// The COMPOSED candidate snapshot digest that verification bound and
    /// landing reproduced exactly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_snapshot: Option<String>,
    /// The FRESH whole-root digest taken after the landing equals
    /// [`Self::final_snapshot_hash`]. Explicit field; never derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_snapshot: Option<String>,
    /// Digest of the proof basis the verification record was created under
    /// (the same digest the record's environment fingerprint carries), so
    /// the integration record names the exact reuse basis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_basis_digest: Option<String>,
    /// Deterministic identity of the landing transaction that produced the
    /// landed snapshot ([`IntegrationTxnRow::txn_id`]); `None` when no
    /// landing transaction ran (blocked / pre-landing rows).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_txn_id: Option<String>,
    pub final_root: String,
    /// Lowercase 64-hex BLAKE3 digest of the final integration root; EMPTY
    /// while the record is in-flight (record-first, before any apply).
    pub final_snapshot_hash: String,
    /// A bounded, deterministic sample of the integrated file paths (the
    /// FULL list is covered by `integrated_files_digest`).
    #[serde(default)]
    pub integrated_files: Vec<String>,
    /// Exact number of files the integration applied (0 = no mutating
    /// children / no changes).
    pub integrated_file_count: u64,
    /// Deterministic digest of the FULL sorted integrated-file list
    /// (empty when no files were integrated). Never a silent truncation:
    /// the sample plus the digest plus the count are the honest bounds.
    #[serde(default)]
    pub integrated_files_digest: String,
    /// A bounded sample of the conflict summaries that blocked the
    /// integration (empty = clean).
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Exact number of conflicts surfaced.
    pub conflict_count: u64,
    pub sources: Vec<IntegrationSourceRow>,
    /// Exact number of sources (may exceed `sources.len()`).
    #[serde(default)]
    pub source_count: u64,
    /// Digest of the FULL source list, for the bounded case.
    #[serde(default)]
    pub sources_digest: String,
    pub at_ms: i64,
}

/// The durable IMMUTABLE run base of one orchestrated run (prepare phase
/// zero): the snapshot digest of the owner root taken BEFORE the first child
/// spawn, the copied base root and the manifest digest of the copy. Every
/// isolated child derives from this generation and
/// [`crate::ledger`]-backed staging refuses a child whose recorded base does
/// not name this record. Written once per run; a second record for the same
/// run is a typed Conflict at the ledger read (the newest row wins and the
/// executor refuses a digest drift).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBaseRecord {
    pub run_id: String,
    pub workspace_id: u64,
    pub worktree_id: u64,
    /// Lowercase 64-hex BLAKE3 of the owner root at the moment the base was
    /// accepted (before == after == copied by the stable-copy contract).
    pub snapshot_hash: String,
    /// Deterministic digest of the copied manifest (path|hash rows).
    pub manifest_digest: String,
    /// The daemon-owned base root every child/candidate derives from.
    pub root: String,
    pub created_ms: i64,
}

/// One entry of the VERIFIED manifest the publication artifact binds: the
/// workspace-relative path and its canonical entry state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedManifestEntry {
    pub path: String,
    pub state: faktor_fs::entry_state::EntryState,
}

/// The durable VERIFIED-GIT publication artifact of one completion contract
/// revision. The commit step builds it from the immutable verification
/// record's tree hash and the verified root manifest, then records the
/// exact git tree/commit and the local/remote refs each later step must
/// re-assert (`git_tree_oid`, `commit_oid`, `local_ref`,
/// `remote_ref` — the encoded exact remote head).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedGitArtifact {
    pub task_id: u64,
    pub revision: u64,
    /// The immutable verification record this artifact certifies.
    pub verification_record: u64,
    /// The exact `tm1:`/snapshot digest of the verified root the manifest
    /// was taken from.
    pub verified_root_digest: String,
    /// The verified manifest, entry for entry.
    pub verified_manifest: Vec<VerifiedManifestEntry>,
    pub git_tree_oid: Option<String>,
    pub commit_oid: Option<String>,
    /// The local branch ref the commit moved (e.g. `refs/heads/main`).
    pub local_ref: Option<String>,
    /// The EXACT published remote ref, encoded
    /// `<remote>:<refname>@<oid>` (produced only after a push reconciled
    /// the live remote ref to the exact commit).
    pub remote_ref: Option<String>,
    pub updated_ms: i64,
}

/// The exact INPUT IDENTITY of one external operation: the tuple a
/// provider-side reconciliation lookup is keyed by
/// (organization/repository + exact head/base + a stable Faktor task
/// marker). Two rows may share one operation key ONLY while this value is
/// byte-identical; any drift is a typed refusal, never a silent second
/// remote object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalOperationInput {
    pub organization: String,
    pub repository: String,
    /// The head of the operation (a branch ref, a commit sha, ...).
    pub head: String,
    /// The base of the operation; empty for operations that have no base.
    #[serde(default)]
    pub base: String,
    /// The stable Faktor task marker the provider lookup carries (never a
    /// random per-call value).
    pub marker: String,
}

/// The lifecycle state of one durable external operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalOperationState {
    /// Recorded BEFORE the remote call: the call may or may not have
    /// happened, so a restart MUST reconcile (never blindly re-create).
    Prepared,
    /// The remote object identity (id + version) is recorded.
    Completed,
    /// The remote call failed typed; no remote object is known to exist.
    Failed,
}

impl ExternalOperationState {
    /// The stable wire/durable tag of this state (also the JSON tag).
    pub const fn as_tag(self) -> &'static str {
        match self {
            ExternalOperationState::Prepared => "prepared",
            ExternalOperationState::Completed => "completed",
            ExternalOperationState::Failed => "failed",
        }
    }
}

/// The durable identity + reconciliation state of ONE external operation.
/// The row is append-only evidence: `Prepared` is journaled before the
/// remote call, the matching `Completed`/`Failed` row after it, and every
/// reader folds the LATEST row of an operation key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalOperationRow {
    /// Deterministic content id over (operation key, provider, kind, input
    /// identity) — never a random uuid, never a caller-supplied value.
    pub id: String,
    /// The stable operation key (`task:<id>:rev:<rev>:<provider>:<kind>`):
    /// exactly one operation per key; a later attempt with a DIFFERENT input
    /// identity under the same key is a typed conflict.
    pub operation_key: String,
    pub provider: String,
    pub kind: String,
    pub input: ExternalOperationInput,
    pub state: ExternalOperationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_object_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_object_version: Option<String>,
    pub started_at: i64,
    /// Set when the completing row was produced by reconciling a `Prepared`
    /// row after a crash (proving the remote call was re-derived, not
    /// blindly repeated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reconciled_at: Option<i64>,
}

impl ExternalOperationRow {
    /// The deterministic content id of one operation row: a BLAKE3 digest
    /// over the operation key, provider/kind and the exact input identity,
    /// with length-prefixed legs so no two distinct tuples can collide.
    pub fn content_id(
        operation_key: &str,
        provider: &str,
        kind: &str,
        input: &ExternalOperationInput,
    ) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor-external-operation:v1\0");
        for part in [
            operation_key.as_bytes(),
            provider.as_bytes(),
            kind.as_bytes(),
            input.organization.as_bytes(),
            input.repository.as_bytes(),
            input.head.as_bytes(),
            input.base.as_bytes(),
            input.marker.as_bytes(),
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }
}

/// Shape bounds of one durable external-operation row, shared by the
/// appender and the strict decoder (a hostile raw row fails loudly on read
/// too). Coherence rules: a `Prepared` row carries NO remote identity, a
/// `Completed` row carries BOTH remote id and version, a `Failed` row names
/// no remote object, and the deterministic id must match the row content.
fn validate_external_operation(row: &ExternalOperationRow) -> Result<(), SessionError> {
    for (what, value, max) in [
        ("id", row.id.as_str(), MAX_EXTERNAL_OPERATION_TEXT_BYTES),
        (
            "operation_key",
            row.operation_key.as_str(),
            MAX_EXTERNAL_OPERATION_TEXT_BYTES,
        ),
        (
            "provider",
            row.provider.as_str(),
            MAX_EXTERNAL_OPERATION_TEXT_BYTES,
        ),
        ("kind", row.kind.as_str(), MAX_EXTERNAL_OPERATION_TEXT_BYTES),
        (
            "input.marker",
            row.input.marker.as_str(),
            MAX_EXTERNAL_OPERATION_TEXT_BYTES,
        ),
        (
            "input.organization",
            row.input.organization.as_str(),
            MAX_EXTERNAL_OPERATION_REF_BYTES,
        ),
        (
            "input.repository",
            row.input.repository.as_str(),
            MAX_EXTERNAL_OPERATION_REF_BYTES,
        ),
        (
            "input.head",
            row.input.head.as_str(),
            MAX_EXTERNAL_OPERATION_REF_BYTES,
        ),
        (
            "input.base",
            row.input.base.as_str(),
            MAX_EXTERNAL_OPERATION_REF_BYTES,
        ),
    ] {
        if value.is_empty() || value.len() > max {
            return Err(SessionError::Malformed(format!(
                "ledger external_operation {what} must be 1..={max} bytes"
            )));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(SessionError::Malformed(format!(
                "ledger external_operation {what} carries control characters"
            )));
        }
    }
    for (what, value, max) in [
        (
            "remote_object_id",
            row.remote_object_id.as_deref(),
            MAX_EXTERNAL_OPERATION_TEXT_BYTES,
        ),
        (
            "remote_object_version",
            row.remote_object_version.as_deref(),
            MAX_EXTERNAL_OPERATION_TEXT_BYTES,
        ),
    ] {
        if let Some(value) = value {
            if value.is_empty() || value.len() > max || value.chars().any(|c| c.is_control()) {
                return Err(SessionError::Malformed(format!(
                    "ledger external_operation {what} must be 1..={max} bytes without control characters"
                )));
            }
        }
    }
    match row.state {
        ExternalOperationState::Prepared => {
            if row.remote_object_id.is_some() || row.remote_object_version.is_some() {
                return Err(SessionError::Malformed(
                    "ledger external_operation prepared row may not carry a remote object identity"
                        .into(),
                ));
            }
            if row.reconciled_at.is_some() {
                return Err(SessionError::Malformed(
                    "ledger external_operation prepared row may not carry reconciled_at".into(),
                ));
            }
        }
        ExternalOperationState::Completed => {
            if row.remote_object_id.is_none() || row.remote_object_version.is_none() {
                return Err(SessionError::Malformed(
                    "ledger external_operation completed row requires remote_object_id and remote_object_version"
                        .into(),
                ));
            }
        }
        ExternalOperationState::Failed => {
            if row.remote_object_id.is_some() || row.remote_object_version.is_some() {
                return Err(SessionError::Malformed(
                    "ledger external_operation failed row may not carry a remote object identity"
                        .into(),
                ));
            }
        }
    }
    if row.started_at <= 0 {
        return Err(SessionError::Malformed(
            "ledger external_operation started_at must be positive".into(),
        ));
    }
    if let Some(reconciled_at) = row.reconciled_at {
        if reconciled_at < row.started_at {
            return Err(SessionError::Malformed(
                "ledger external_operation reconciled_at precedes started_at".into(),
            ));
        }
    }
    let expected =
        ExternalOperationRow::content_id(&row.operation_key, &row.provider, &row.kind, &row.input);
    if row.id != expected {
        return Err(SessionError::Malformed(format!(
            "ledger external_operation id {} is not the deterministic content id {expected} of its key/provider/kind/input",
            row.id
        )));
    }
    Ok(())
}

/// The durable phase of one transactional owner landing (record-first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationTxnPhase {
    /// Decisions + rollback blobs recorded, verification not yet bound.
    Prepared,
    /// The verified candidate snapshot is bound; owner not touched.
    Verified,
    /// The owner-equality recheck passed; per-path applies in flight.
    Landing,
    /// Every path applied and the landed root equals the verified candidate.
    Landed,
    /// A late conflict forced a rollback of the paths we applied.
    RollingBack,
    /// Every path restored to base (or marked a rollback conflict).
    RolledBack,
    /// Landing refused deterministically (owner drift before the first
    /// write); nothing was applied.
    Blocked,
}

impl IntegrationTxnPhase {
    pub fn as_tag(self) -> &'static str {
        match self {
            IntegrationTxnPhase::Prepared => "prepared",
            IntegrationTxnPhase::Verified => "verified",
            IntegrationTxnPhase::Landing => "landing",
            IntegrationTxnPhase::Landed => "landed",
            IntegrationTxnPhase::RollingBack => "rolling_back",
            IntegrationTxnPhase::RolledBack => "rolled_back",
            IntegrationTxnPhase::Blocked => "blocked",
        }
    }

    /// Whether the phase is finished (no recovery action may run).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            IntegrationTxnPhase::Landed
                | IntegrationTxnPhase::RolledBack
                | IntegrationTxnPhase::Blocked
        )
    }
}

/// The durable per-path state of one landing transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationPathTxnState {
    Pending,
    Applied,
    RolledBack,
    Conflict,
    RollbackConflict,
}

/// ONE per-path landing decision + outcome in the CANONICAL entry-state
/// vocabulary: the base state, the verified candidate state, and the
/// rollback CAS material (a regular base payload blob and/or a symlink
/// literal target; the mode is part of the state). A path that was absent at
/// the run base carries [`EntryState::Absent`] — never a `None` hash.
///
/// `canonical` is `false` for a LEGACY byte-only row (pre-entry-state): such
/// a row decodes additively but is NEVER landed or rolled back from, because
/// its byte-only anchors cannot prove kind/mode/target identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationPathTxn {
    pub path: String,
    #[serde(default = "faktor_fs::entry_state::EntryState::absent")]
    pub base_state: faktor_fs::entry_state::EntryState,
    #[serde(default = "faktor_fs::entry_state::EntryState::absent")]
    pub candidate_state: faktor_fs::entry_state::EntryState,
    /// CAS digest of the base REGULAR payload blob (the rollback authority
    /// for a regular base), when one exists.
    #[serde(default)]
    pub rollback_blob: Option<String>,
    /// The base SYMLINK literal target bytes (rollback authority for a
    /// symlink base; carried inline because a literal target is not a CAS
    /// blob), when the base was a symlink.
    #[serde(default)]
    pub rollback_link_target: Option<Vec<u8>>,
    /// True on every newly-written decision row. A legacy `false` row is a
    /// typed refusal at landing/recovery time.
    #[serde(default)]
    pub canonical: bool,
    pub state: IntegrationPathTxnState,
}

impl IntegrationPathTxn {
    pub fn state_tag(&self) -> &'static str {
        match self.state {
            IntegrationPathTxnState::Pending => "pending",
            IntegrationPathTxnState::Applied => "applied",
            IntegrationPathTxnState::RolledBack => "rolled_back",
            IntegrationPathTxnState::Conflict => "conflict",
            IntegrationPathTxnState::RollbackConflict => "rollback_conflict",
        }
    }

    /// True when this row carries the canonical states AND the exact
    /// rollback material its base state requires (a regular base needs its
    /// payload blob, a symlink base its literal target, an absent base
    /// nothing). A legacy or material-incomplete row is never landed from.
    pub fn canonical_ready(&self) -> bool {
        if !self.canonical {
            return false;
        }
        match &self.base_state {
            faktor_fs::entry_state::EntryState::Absent => {
                self.rollback_blob.is_none() && self.rollback_link_target.is_none()
            }
            faktor_fs::entry_state::EntryState::Regular { payload, .. } => self
                .rollback_blob
                .as_deref()
                .and_then(faktor_core::hash::FileHash::from_hex)
                .is_some_and(|blob| blob == *payload),
            faktor_fs::entry_state::EntryState::Symlink { target, .. } => {
                self.rollback_link_target.as_deref() == Some(target.as_slice())
            }
        }
    }
}

/// The durable landing transaction of one orchestrated run: record-first
/// decisions + rollback blobs before the first owner write, per-path
/// outcomes, and the deterministic phase a crashed executor recovers from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationTxnRow {
    pub run_id: String,
    pub task_id: u64,
    pub owner_root: String,
    pub candidate_root: String,
    pub run_base_snapshot: String,
    /// The verified candidate snapshot landing must reproduce exactly.
    pub verified_candidate_snapshot: String,
    pub sources_digest: String,
    pub phase: IntegrationTxnPhase,
    pub paths: Vec<IntegrationPathTxn>,
    /// Exact number of decision rows the transaction carries (the stored
    /// list may be bounded; nothing is silently dropped).
    pub path_count: u64,
    pub applied_count: u64,
    pub conflicts: Vec<String>,
    pub at_ms: i64,
}

impl IntegrationTxnRow {
    /// The deterministic CONTENT identity of this landing transaction: a
    /// `blake3:`-prefixed digest over the immutable transaction legs (run,
    /// task, owner/candidate roots, run base, verified candidate snapshot,
    /// sources digest). It is stable across process restarts and recovery
    /// replays — the mutable phase/path/at_ms fields are deliberately
    /// excluded — so an integration record can name exactly which landing
    /// transaction produced its landed snapshot.
    pub fn txn_id(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor-integration-txn:v1\0");
        for part in [
            self.run_id.as_bytes(),
            &self.task_id.to_le_bytes(),
            self.owner_root.as_bytes(),
            self.candidate_root.as_bytes(),
            self.run_base_snapshot.as_bytes(),
            self.verified_candidate_snapshot.as_bytes(),
            self.sources_digest.as_bytes(),
        ] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    }
}

/// Report of one watermark compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerCompactReport {
    pub entries_before: i64,
    pub deleted: i64,
    pub kept: i64,
    /// The pinned never-evict seqs (last GoalSet/CriteriaSet/Decision +
    /// every unresolved BlockerOpened).
    pub pinned: Vec<i64>,
    /// New head checkpoint seq (the pre-prune max).
    pub checkpoint_seq: i64,
}

// ---------------------------------------------------------------- decode/encode

fn entry_tag_of(payload: &LedgerPayload) -> &'static str {
    match payload {
        LedgerPayload::GoalSet { .. } => ENTRY_GOAL_SET,
        LedgerPayload::CriteriaSet { .. } => ENTRY_CRITERIA_SET,
        LedgerPayload::BlockerOpened { .. } => ENTRY_BLOCKER_OPENED,
        LedgerPayload::BlockerResolved { .. } => ENTRY_BLOCKER_RESOLVED,
        LedgerPayload::Decision { .. } => ENTRY_DECISION,
        LedgerPayload::PlanStepAdded { .. } => ENTRY_PLAN_STEP_ADDED,
        LedgerPayload::ChildAgentStarted { .. } => ENTRY_CHILD_AGENT_STARTED,
        LedgerPayload::ChildAgentFinished { .. } => ENTRY_CHILD_AGENT_FINISHED,
        LedgerPayload::RoutingDecision { .. } => ENTRY_ROUTING_DECISION,
        LedgerPayload::EpochBumped { .. } => ENTRY_EPOCH_BUMPED,
        LedgerPayload::FailureRecorded { .. } => ENTRY_FAILURE_RECORDED,
        LedgerPayload::VerifyRun { .. } => ENTRY_VERIFY_RUN,
        LedgerPayload::TurnCompleted { .. } => ENTRY_TURN_COMPLETED,
        LedgerPayload::LearningRecord { .. } => ENTRY_LEARNING_RECORD,
        LedgerPayload::EditTxnPrepared { .. } => ENTRY_EDIT_TXN_PREPARED,
        LedgerPayload::EditTxnProgress { .. } => ENTRY_EDIT_TXN_PROGRESS,
        LedgerPayload::EditTxnCommitted { .. } => ENTRY_EDIT_TXN_COMMITTED,
        LedgerPayload::EditTxnRolledBack { .. } => ENTRY_EDIT_TXN_ROLLED_BACK,
        LedgerPayload::TournamentStarted { .. } => ENTRY_TOURNAMENT_STARTED,
        LedgerPayload::CandidateSettled { .. } => ENTRY_CANDIDATE_SETTLED,
        LedgerPayload::TournamentDecided { .. } => ENTRY_TOURNAMENT_DECIDED,
        LedgerPayload::ChildPresentationChanged { .. } => ENTRY_CHILD_PRESENTATION_CHANGED,
        LedgerPayload::BoardPost { .. } => ENTRY_BOARD_POST,
        LedgerPayload::BoardRead { .. } => ENTRY_BOARD_READ,
        LedgerPayload::BoardReceipt { .. } => ENTRY_BOARD_RECEIPT,
        LedgerPayload::BoardReset { .. } => ENTRY_BOARD_RESET,
        LedgerPayload::CompletionContractSet { .. } => ENTRY_COMPLETION_CONTRACT_SET,
        LedgerPayload::CompletionStepStatus { .. } => ENTRY_COMPLETION_STEP_STATUS,
        LedgerPayload::IntegrationRecorded { .. } => ENTRY_INTEGRATION_RECORD,
        LedgerPayload::RunBaseRecorded { .. } => ENTRY_RUN_BASE,
        LedgerPayload::VerifiedGitArtifactRecorded { .. } => ENTRY_VERIFIED_GIT_ARTIFACT,
        LedgerPayload::IntegrationTxnRecorded { .. } => ENTRY_INTEGRATION_TXN,
        LedgerPayload::ExternalOperationRecorded { .. } => ENTRY_EXTERNAL_OPERATION,
        LedgerPayload::TerminalCreated { .. } => ENTRY_TERMINAL_CREATED,
        LedgerPayload::TerminalRunning { .. } => ENTRY_TERMINAL_RUNNING,
        LedgerPayload::TerminalExited { .. } => ENTRY_TERMINAL_EXITED,
        LedgerPayload::TerminalKilled { .. } => ENTRY_TERMINAL_KILLED,
        LedgerPayload::TerminalLost { .. } => ENTRY_TERMINAL_LOST,
        LedgerPayload::TerminalReconciled { .. } => ENTRY_TERMINAL_RECONCILED,
    }
}

/// Project one decoded typed entry onto a durable terminal record (`None`
/// for every non-terminal entry). The record's `kind` is the entry tag and
/// its `detail` the kind's bounded audit text.
fn terminal_record_of(entry: TypedLedgerEntry) -> Option<TerminalLedgerRecord> {
    let (kind, row, exit_code, detail) = match entry.payload {
        LedgerPayload::TerminalCreated { row } => {
            (TerminalEventKind::Created, row, None, String::new())
        }
        LedgerPayload::TerminalRunning { row } => {
            (TerminalEventKind::Running, row, None, String::new())
        }
        LedgerPayload::TerminalExited { row, exit_code } => {
            (TerminalEventKind::Exited, row, exit_code, String::new())
        }
        LedgerPayload::TerminalKilled { row, reason } => {
            (TerminalEventKind::Killed, row, None, reason)
        }
        LedgerPayload::TerminalLost { row, reason } => (TerminalEventKind::Lost, row, None, reason),
        LedgerPayload::TerminalReconciled { row, disposition } => {
            (TerminalEventKind::Reconciled, row, None, disposition)
        }
        _ => return None,
    };
    Some(TerminalLedgerRecord {
        seq: entry.seq,
        kind,
        row,
        exit_code,
        detail,
    })
}

/// Decode one typed payload from its row. Unknown `entry_type` or unknown
/// `schema_ver` => loud `Malformed` (corrupt), never a silent parse.
/// Schema-shape violations are equally loud.
fn decode_payload(
    entry_type: &str,
    schema_ver: i64,
    json: &serde_json::Value,
) -> Result<LedgerPayload, SessionError> {
    if schema_ver != LEDGER_ENTRY_SCHEMA_V {
        return Err(SessionError::Malformed(format!(
            "ledger entry {entry_type:?} has unknown schema version {schema_ver} \
             (this reader understands v{LEDGER_ENTRY_SCHEMA_V}); refusing to parse"
        )));
    }
    let decode = |tag: &str| -> Result<LedgerPayload, SessionError> {
        serde_json::from_value(json.clone()).map_err(|e| {
            SessionError::Malformed(format!(
                "ledger entry {tag} payload violates its v1 schema: {e}"
            ))
        })
    };
    match entry_type {
        ENTRY_GOAL_SET => decode(entry_type),
        ENTRY_CRITERIA_SET => decode(entry_type),
        ENTRY_BLOCKER_OPENED => decode(entry_type),
        ENTRY_BLOCKER_RESOLVED => decode(entry_type),
        ENTRY_DECISION => decode(entry_type),
        ENTRY_PLAN_STEP_ADDED => decode(entry_type),
        ENTRY_CHILD_AGENT_STARTED => decode(entry_type),
        ENTRY_CHILD_AGENT_FINISHED => decode(entry_type),
        ENTRY_ROUTING_DECISION => decode(entry_type),
        ENTRY_EPOCH_BUMPED => decode(entry_type),
        ENTRY_FAILURE_RECORDED => decode(entry_type),
        ENTRY_VERIFY_RUN => decode(entry_type),
        ENTRY_TURN_COMPLETED => decode(entry_type),
        ENTRY_LEARNING_RECORD => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::LearningRecord { record, payload } = &decoded {
                validate_learning_record(record, payload)?;
            }
            Ok(decoded)
        }
        ENTRY_EDIT_TXN_PREPARED => decode(entry_type),
        ENTRY_EDIT_TXN_PROGRESS => decode(entry_type),
        ENTRY_EDIT_TXN_COMMITTED => decode(entry_type),
        ENTRY_EDIT_TXN_ROLLED_BACK => decode(entry_type),
        ENTRY_TOURNAMENT_STARTED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::TournamentStarted {
                tournament_id,
                run_family,
                goal,
                criteria,
                candidates,
            } = &decoded
            {
                validate_tournament_started(tournament_id, run_family, goal, criteria, candidates)?;
            }
            Ok(decoded)
        }
        ENTRY_CANDIDATE_SETTLED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CandidateSettled {
                tournament_id,
                settlement,
            } = &decoded
            {
                validate_tournament_id(tournament_id, "tournament id")?;
                validate_tournament_settlement(settlement)?;
            }
            Ok(decoded)
        }
        ENTRY_TOURNAMENT_DECIDED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::TournamentDecided {
                tournament_id,
                winner,
                outcome,
                rationale,
            } = &decoded
            {
                validate_tournament_id(tournament_id, "tournament id")?;
                if let Some(w) = winner {
                    validate_tournament_text(w, "tournament winner")?;
                }
                if !matches!(
                    outcome.as_str(),
                    TOURNAMENT_OUTCOME_DECIDED | TOURNAMENT_OUTCOME_ABORTED
                ) {
                    return Err(SessionError::Malformed(format!(
                        "ledger tournament_decided outcome {outcome:?} is not decided|aborted"
                    )));
                }
                if outcome == TOURNAMENT_OUTCOME_DECIDED && winner.is_none() {
                    return Err(SessionError::Malformed(
                        "ledger tournament_decided with outcome decided requires a winner".into(),
                    ));
                }
                if rationale.is_empty() || rationale.len() > MAX_TOURNAMENT_OUTCOME {
                    return Err(SessionError::Malformed(
                        "ledger tournament_decided rationale must be 1..=MAX_TOURNAMENT_OUTCOME bytes"
                            .into(),
                    ));
                }
            }
            Ok(decoded)
        }
        ENTRY_CHILD_PRESENTATION_CHANGED => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::ChildPresentationChanged {
                child_id,
                from,
                to,
                at_ms,
            } = &decoded
            {
                validate_child_presentation(child_id, *from, *to, *at_ms)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_POST => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardPost {
                board_id,
                post_id,
                author_child,
                author_session,
                subject,
                body,
                refs,
                revision,
            } = &decoded
            {
                validate_board_post(
                    *board_id,
                    *post_id,
                    *author_child,
                    *author_session,
                    subject,
                    body,
                    refs,
                    *revision,
                )?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_READ => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardRead {
                board_id,
                child,
                post_id,
            } = &decoded
            {
                validate_board_read(*board_id, *child, *post_id)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_RECEIPT => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardReceipt {
                board_id,
                child,
                post_id,
                action,
                note,
            } = &decoded
            {
                validate_board_receipt(*board_id, *child, *post_id, action, note)?;
            }
            Ok(decoded)
        }
        ENTRY_BOARD_RESET => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::BoardReset {
                board_id,
                previous_revision,
                new_revision,
            } = &decoded
            {
                validate_board_reset(*board_id, *previous_revision, *new_revision)?;
            }
            Ok(decoded)
        }
        ENTRY_COMPLETION_CONTRACT_SET => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CompletionContractSet {
                task_id,
                revision,
                contract,
            } = &decoded
            {
                validate_completion_contract_set(*task_id, *revision, contract)?;
            }
            Ok(decoded)
        }
        ENTRY_COMPLETION_STEP_STATUS => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::CompletionStepStatus {
                task_id,
                revision,
                detail,
                at_ms,
                snapshot,
                ..
            } = &decoded
            {
                validate_completion_step_status(
                    *task_id,
                    *revision,
                    detail,
                    *at_ms,
                    snapshot.as_deref(),
                )?;
            }
            Ok(decoded)
        }
        ENTRY_INTEGRATION_RECORD => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::IntegrationRecorded { record } = &decoded {
                validate_integration_record(record)?;
            }
            Ok(decoded)
        }
        ENTRY_RUN_BASE => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::RunBaseRecorded { record } = &decoded {
                validate_run_base_record(record)?;
            }
            Ok(decoded)
        }
        ENTRY_INTEGRATION_TXN => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::IntegrationTxnRecorded { row } = &decoded {
                validate_integration_txn(row)?;
            }
            Ok(decoded)
        }
        ENTRY_VERIFIED_GIT_ARTIFACT => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::VerifiedGitArtifactRecorded { artifact } = &decoded {
                validate_verified_git_artifact(artifact)?;
            }
            Ok(decoded)
        }
        ENTRY_EXTERNAL_OPERATION => {
            let decoded = decode(entry_type)?;
            if let LedgerPayload::ExternalOperationRecorded { record } = &decoded {
                validate_external_operation(record)?;
            }
            Ok(decoded)
        }
        ENTRY_TERMINAL_CREATED
        | ENTRY_TERMINAL_RUNNING
        | ENTRY_TERMINAL_EXITED
        | ENTRY_TERMINAL_KILLED
        | ENTRY_TERMINAL_LOST
        | ENTRY_TERMINAL_RECONCILED => {
            let decoded = decode(entry_type)?;
            match &decoded {
                LedgerPayload::TerminalCreated { row } => {
                    validate_terminal_row(row, "terminal_created")?
                }
                LedgerPayload::TerminalRunning { row } => {
                    validate_terminal_row(row, "terminal_running")?
                }
                LedgerPayload::TerminalExited { row, .. } => {
                    validate_terminal_row(row, "terminal_exited")?
                }
                LedgerPayload::TerminalKilled { row, reason } => {
                    validate_terminal_row(row, "terminal_killed")?;
                    validate_terminal_detail(reason, "terminal_killed reason")?;
                }
                LedgerPayload::TerminalLost { row, reason } => {
                    validate_terminal_row(row, "terminal_lost")?;
                    validate_terminal_detail(reason, "terminal_lost reason")?;
                }
                LedgerPayload::TerminalReconciled { row, disposition } => {
                    validate_terminal_row(row, "terminal_reconciled")?;
                    if !matches!(
                        disposition.as_str(),
                        TERMINAL_RECONCILE_KILLED | TERMINAL_RECONCILE_COLLECTED
                    ) {
                        return Err(SessionError::Malformed(format!(
                            "ledger terminal_reconciled disposition {disposition:?} is not \
                             killed|collected"
                        )));
                    }
                }
                _ => {
                    return Err(SessionError::Internal(
                        "terminal entry decode returned a non-terminal payload".into(),
                    ))
                }
            }
            Ok(decoded)
        }
        other => Err(SessionError::Malformed(format!(
            "ledger entry type {other:?} is unknown to this reader"
        ))),
    }
}

/// Shape bounds of one durable terminal row, shared by every appender and
/// the strict decoder (a hostile raw row fails loudly on read too). The
/// terminal UUID is the authority key and is never pid-derived; the pid and
/// the OS start time are the process identity (a start time of 0 = unknown,
/// which recovery treats as unverifiable — never "alive by pid").
fn validate_terminal_row(row: &TerminalDurableRow, what: &str) -> Result<(), SessionError> {
    if row.terminal_id.is_empty() || row.terminal_id.len() > MAX_TERMINAL_ID_BYTES {
        return Err(SessionError::Malformed(format!(
            "ledger {what} terminal_id must be 1..={MAX_TERMINAL_ID_BYTES} bytes"
        )));
    }
    if !row.terminal_id.is_ascii()
        || row.terminal_id.contains('/')
        || row.terminal_id.contains('\\')
        || row.terminal_id.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} terminal_id must be printable ASCII without '/' or '\\'"
        )));
    }
    if row.session_id == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} session_id must be non-zero"
        )));
    }
    if row.operation_id == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} operation_id must be non-zero"
        )));
    }
    if row.pid == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} pid must be non-zero"
        )));
    }
    if row.start_time_ms < 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} start_time_ms must be >= 0 (0 = unverified identity)"
        )));
    }
    if row.at_ms <= 0 {
        return Err(SessionError::Malformed(format!(
            "ledger {what} at_ms must be positive"
        )));
    }
    if let Some(agent_id) = &row.agent_id {
        if agent_id.is_empty() || agent_id.len() > MAX_TERMINAL_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger {what} agent_id must be 1..={MAX_TERMINAL_ID_BYTES} bytes when set"
            )));
        }
        if !agent_id.is_ascii() || agent_id.chars().any(|c| c.is_control()) {
            return Err(SessionError::Malformed(format!(
                "ledger {what} agent_id must be printable ASCII"
            )));
        }
    }
    // The effective execution profile is bounded audit evidence: empty means
    // a legacy row (readable), a set value must never be hostile.
    if row.execution_profile.len() > MAX_TERMINAL_PROFILE_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger {what} execution_profile of {} bytes exceeds {MAX_TERMINAL_PROFILE_BYTES}",
            row.execution_profile.len()
        )));
    }
    if row.execution_profile.contains('\0')
        || row
            .execution_profile
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} execution_profile contains control/NUL characters"
        )));
    }
    Ok(())
}

/// One bounded terminal audit detail (kill/lost reason). Non-empty and no
/// NUL: the detail lands in operator-visible projections.
fn validate_terminal_detail(detail: &str, what: &str) -> Result<(), SessionError> {
    if detail.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be non-empty"
        )));
    }
    if detail.len() > MAX_TERMINAL_DETAIL_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger {what} of {} bytes exceeds {MAX_TERMINAL_DETAIL_BYTES}",
            detail.len()
        )));
    }
    if detail.contains('\0') {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must not contain NUL"
        )));
    }
    Ok(())
}

/// Shape bounds of one `learning_record` row. Shared by the appender
/// (rejects BEFORE any byte is journaled) and the decoder (a hostile raw
/// row must fail loudly on read too). The payload's inner schema is the
/// learning crate's; this layer owns only its kind and its bound.
fn validate_learning_record(record: &str, payload: &str) -> Result<(), SessionError> {
    if !matches!(
        record,
        LEARNING_RECORD_EPISODE | LEARNING_RECORD_LEARNING | LEARNING_RECORD_REMOVED
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger learning record kind {record:?} is not episode|learning|removed"
        )));
    }
    if payload.is_empty() {
        return Err(SessionError::Malformed(
            "ledger learning record payload must be non-empty".into(),
        ));
    }
    if payload.len() > MAX_LEARNING_RECORD_PAYLOAD {
        return Err(SessionError::Oversized(format!(
            "ledger learning record payload of {} bytes exceeds MAX_LEARNING_RECORD_PAYLOAD",
            payload.len()
        )));
    }
    Ok(())
}

fn check_text(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger entry {what} must be non-empty"
        )));
    }
    if value.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(format!(
            "ledger entry {what} of {} bytes exceeds {MAX_LEDGER_TEXT}",
            value.len()
        )));
    }
    Ok(())
}

fn validate_tournament_id(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() || value.len() > MAX_TOURNAMENT_ID {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be 1..={MAX_TOURNAMENT_ID} bytes"
        )));
    }
    if !value.is_ascii()
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be printable ASCII without '/' or '\\\\'"
        )));
    }
    Ok(())
}

fn validate_tournament_text(value: &str, what: &str) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger {what} must be non-empty"
        )));
    }
    if value.len() > MAX_TOURNAMENT_TEXT {
        return Err(SessionError::Oversized(format!(
            "ledger {what} of {} bytes exceeds MAX_TOURNAMENT_TEXT ({MAX_TOURNAMENT_TEXT})",
            value.len()
        )));
    }
    Ok(())
}

fn validate_tournament_checks(checks: &[TournamentCheckSpec]) -> Result<(), SessionError> {
    if checks.is_empty() {
        return Err(SessionError::Malformed(
            "ledger candidate settlement requires the derived check set".into(),
        ));
    }
    if checks.len() > MAX_TOURNAMENT_CRITERIA {
        return Err(SessionError::Oversized(format!(
            "ledger candidate settlement of {} checks exceeds MAX_TOURNAMENT_CRITERIA",
            checks.len()
        )));
    }
    for c in checks {
        validate_tournament_text(&c.id, "tournament check id")?;
        validate_tournament_text(&c.spec, "tournament check spec")?;
    }
    Ok(())
}

fn validate_tournament_started(
    tournament_id: &str,
    run_family: &str,
    goal: &str,
    criteria: &[TournamentCriterionRow],
    candidates: &[TournamentCandidateRow],
) -> Result<(), SessionError> {
    validate_tournament_id(tournament_id, "tournament id")?;
    validate_tournament_id(run_family, "tournament run family")?;
    validate_tournament_text(goal, "tournament goal")?;
    if criteria.is_empty() || criteria.len() > MAX_TOURNAMENT_CRITERIA {
        return Err(SessionError::Malformed(format!(
            "ledger tournament_started must carry 1..={MAX_TOURNAMENT_CRITERIA} criteria"
        )));
    }
    let mut seen_criteria: Vec<&str> = Vec::with_capacity(criteria.len());
    for c in criteria {
        validate_tournament_text(&c.id, "tournament criterion id")?;
        validate_tournament_text(&c.spec, "tournament criterion spec")?;
        if seen_criteria.contains(&c.id.as_str()) {
            return Err(SessionError::Malformed(format!(
                "ledger tournament_started carries duplicate criterion id {:?}",
                c.id
            )));
        }
        seen_criteria.push(&c.id);
    }
    if !(MIN_TOURNAMENT_CANDIDATES..=MAX_TOURNAMENT_CANDIDATES).contains(&candidates.len()) {
        return Err(SessionError::Malformed(format!(
            "ledger tournament_started carries {} candidates outside the supported band {MIN_TOURNAMENT_CANDIDATES}..={MAX_TOURNAMENT_CANDIDATES}",
            candidates.len()
        )));
    }
    let mut seen_children: Vec<&str> = Vec::with_capacity(candidates.len());
    for c in candidates {
        validate_tournament_id(&c.child_id, "tournament candidate child id")?;
        if !c.worktree.is_empty() && c.worktree.len() > MAX_LEDGER_TEXT {
            return Err(SessionError::Oversized(
                "ledger tournament candidate worktree exceeds MAX_LEDGER_TEXT".into(),
            ));
        }
        if c.base_revision.len() > MAX_LEDGER_TEXT {
            return Err(SessionError::Oversized(
                "ledger tournament candidate base revision exceeds MAX_LEDGER_TEXT".into(),
            ));
        }
        if seen_children.contains(&c.child_id.as_str()) {
            return Err(SessionError::Malformed(format!(
                "ledger tournament_started carries duplicate candidate child id {:?}",
                c.child_id
            )));
        }
        seen_children.push(&c.child_id);
    }
    Ok(())
}

fn validate_tournament_settlement(
    settlement: &TournamentSettlementRow,
) -> Result<(), SessionError> {
    validate_tournament_id(&settlement.child_id, "candidate child id")?;
    if settlement.worktree.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(
            "ledger candidate settlement worktree exceeds MAX_LEDGER_TEXT".into(),
        ));
    }
    if settlement.base_revision.len() > MAX_LEDGER_TEXT {
        return Err(SessionError::Oversized(
            "ledger candidate settlement base revision exceeds MAX_LEDGER_TEXT".into(),
        ));
    }
    if !matches!(
        settlement.state.as_str(),
        TOURNAMENT_STATE_DONE | TOURNAMENT_STATE_FAILED | TOURNAMENT_STATE_CANCELLED
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger candidate settlement state {:?} is not done|failed|cancelled",
            settlement.state
        )));
    }
    if settlement.verification == Some(0) {
        return Err(SessionError::Malformed(
            "ledger candidate settlement verification record id cannot be 0".into(),
        ));
    }
    validate_tournament_checks(&settlement.checks)?;
    match (&settlement.review, &settlement.reviewer) {
        (Some(rank), Some(reviewer)) => {
            if !matches!(
                rank.as_str(),
                TOURNAMENT_REVIEW_BLOCK | TOURNAMENT_REVIEW_CONCERN | TOURNAMENT_REVIEW_CLEAN
            ) {
                return Err(SessionError::Malformed(format!(
                    "ledger candidate review {rank:?} is not block|concern|clean"
                )));
            }
            validate_tournament_id(reviewer, "candidate reviewer")?;
        }
        (None, None) => {}
        (Some(_), None) => {
            return Err(SessionError::Malformed(
                "ledger candidate review requires its reviewer identity".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(SessionError::Malformed(
                "ledger candidate settlement carries a reviewer without a review verdict".into(),
            ));
        }
    }
    if settlement.reason.len() > MAX_TOURNAMENT_OUTCOME {
        return Err(SessionError::Oversized(
            "ledger candidate settlement reason exceeds MAX_TOURNAMENT_OUTCOME".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- presentation bounds

/// Shape bounds of one `child_presentation_changed` row, shared by the
/// appender and the strict decoder (a hostile raw row must fail loudly on
/// read too). A same-state transition is refused: the appender treats it as
/// an idempotent no-op and never writes a row, so a durable `from == to`
/// row is corruption.
fn validate_child_presentation(
    child_id: &str,
    from: PresentationState,
    to: PresentationState,
    at_ms: i64,
) -> Result<(), SessionError> {
    if child_id.is_empty() || child_id.len() > MAX_PRESENTATION_CHILD_ID {
        return Err(SessionError::Malformed(format!(
            "ledger child_presentation_changed child_id must be 1..={MAX_PRESENTATION_CHILD_ID} bytes"
        )));
    }
    if !child_id.is_ascii()
        || child_id.contains('/')
        || child_id.contains('\\')
        || child_id.chars().any(|c| c.is_control())
    {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed child_id must be printable ASCII without '/' or '\\'"
                .into(),
        ));
    }
    if at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed at_ms must be positive".into(),
        ));
    }
    if from == to {
        return Err(SessionError::Malformed(
            "ledger child_presentation_changed refuses a no-op transition (from == to): the \
             idempotent path writes no row"
                .into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- board bounds checks

fn check_board_id(value: u64, what: &str) -> Result<(), SessionError> {
    if value == 0 {
        return Err(SessionError::Malformed(format!(
            "ledger board {what} must be non-zero"
        )));
    }
    Ok(())
}

/// One board text field: non-empty, bounded, and free of control characters
/// except the layout characters a coordination message legitimately carries
/// (newline / carriage return / tab).
fn check_board_text(field: &str, value: &str, max: usize) -> Result<(), SessionError> {
    if value.is_empty() {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} must be non-empty"
        )));
    }
    if value.len() > max {
        return Err(SessionError::Oversized(format!(
            "ledger board {field} of {} bytes exceeds {max}",
            value.len()
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} carries control characters"
        )));
    }
    Ok(())
}

/// Board note fields may be empty (an `ack` needs no prose) but carry the
/// same bound and control-character rule.
fn check_board_note(field: &str, value: &str, max: usize) -> Result<(), SessionError> {
    if value.len() > max {
        return Err(SessionError::Oversized(format!(
            "ledger board {field} of {} bytes exceeds {max}",
            value.len()
        )));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(SessionError::Malformed(format!(
            "ledger board {field} carries control characters"
        )));
    }
    Ok(())
}

/// Shape bounds of one `board_post` row, shared by the appender and the
/// strict decoder (a hostile raw row must fail loudly on read too).
#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_board_post(
    board_id: u64,
    post_id: u64,
    author_child: Option<u64>,
    author_session: u64,
    subject: &str,
    body: &str,
    refs: &[String],
    revision: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    check_board_id(post_id, "post_id")?;
    check_board_id(author_session, "author_session")?;
    if author_child == Some(0) {
        return Err(SessionError::Malformed(
            "ledger board author_child cannot be 0 (None means the root agent)".into(),
        ));
    }
    check_board_text("subject", subject, MAX_BOARD_SUBJECT_BYTES)?;
    check_board_text("body", body, MAX_BOARD_BODY_BYTES)?;
    if refs.len() > MAX_BOARD_REFS {
        return Err(SessionError::Oversized(format!(
            "ledger board post of {} refs exceeds MAX_BOARD_REFS",
            refs.len()
        )));
    }
    for r in refs {
        check_board_text("ref", r, MAX_BOARD_REF_BYTES)?;
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger board post revision must be >= 1".into(),
        ));
    }
    if post_id != revision {
        // The durable post identity IS the revision that created it:
        // revisions are never reused (a reset consumes one), so the id is
        // stable, unique per board and recoverable from the pinned stream.
        return Err(SessionError::Malformed(format!(
            "ledger board post id {post_id} does not equal its revision {revision}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_board_read(
    board_id: u64,
    child: u64,
    post_id: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    check_board_id(child, "read child")?;
    check_board_id(post_id, "post_id")?;
    Ok(())
}

pub(crate) fn validate_board_receipt(
    board_id: u64,
    child: Option<u64>,
    post_id: u64,
    action: &str,
    note: &str,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    if child == Some(0) {
        return Err(SessionError::Malformed(
            "ledger board receipt child cannot be 0 (None means the root agent)".into(),
        ));
    }
    check_board_id(post_id, "post_id")?;
    if !matches!(
        action,
        BOARD_RECEIPT_ACK
            | BOARD_RECEIPT_TASK_UPDATE
            | BOARD_RECEIPT_BLOCKED
            | BOARD_RECEIPT_QUESTION
    ) {
        return Err(SessionError::Malformed(format!(
            "ledger board receipt action {action:?} is not ack|task_update|blocked|question"
        )));
    }
    check_board_note("receipt note", note, MAX_BOARD_RECEIPT_NOTE_BYTES)?;
    Ok(())
}

pub(crate) fn validate_board_reset(
    board_id: u64,
    previous_revision: u64,
    new_revision: u64,
) -> Result<(), SessionError> {
    check_board_id(board_id, "board_id")?;
    if previous_revision.checked_add(1) != Some(new_revision) {
        return Err(SessionError::Malformed(format!(
            "ledger board reset must bump the revision by exactly one \
             ({previous_revision} -> {new_revision})"
        )));
    }
    Ok(())
}

/// Shape bounds of one `completion_contract_set` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
/// The default all-false contract is never durable: recording it would
/// create a row whose meaning is "no gate", which the default path already
/// expresses with no row at all.
pub(crate) fn validate_completion_contract_set(
    task_id: u64,
    revision: u64,
    contract: &CompletionContract,
) -> Result<(), SessionError> {
    if task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set task_id must be non-zero".into(),
        ));
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set revision must be >= 1".into(),
        ));
    }
    if contract.is_default() {
        return Err(SessionError::Malformed(
            "ledger completion_contract_set refuses an all-false contract: the default behavior \
             is expressed by the ABSENCE of a row, never by a durable no-op row"
                .into(),
        ));
    }
    Ok(())
}

/// Shape of one stored TREE-snapshot digest: the versioned canonical
/// tree-manifest digest (`tm1:` + 64 hex, `faktor_fs::tree_manifest`) or the
/// legacy 64-char hex content-only digest (accepted so rows written before
/// the canonical manifest still decode). The two shapes can never compare
/// equal, so a legacy record fails equality loudly instead of silently
/// matching the canonical definition.
fn check_snapshot_digest(value: &str, what: &str) -> Result<(), SessionError> {
    let legacy =
        value.len() == MAX_RUN_BASE_DIGEST_BYTES && value.bytes().all(|b| b.is_ascii_hexdigit());
    if legacy || faktor_fs::tree_manifest::is_tree_manifest_digest(value) {
        Ok(())
    } else {
        Err(SessionError::Malformed(format!(
            "ledger {what} must be a canonical `tm1:<64-hex>` tree-manifest digest or a \
             64-char hex legacy digest"
        )))
    }
}

/// Shape bounds of one `completion_step_status` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
pub(crate) fn validate_completion_step_status(
    task_id: u64,
    revision: u64,
    detail: &str,
    at_ms: i64,
    snapshot: Option<&str>,
) -> Result<(), SessionError> {
    if task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status task_id must be non-zero".into(),
        ));
    }
    if revision == 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status revision must be >= 1".into(),
        ));
    }
    if detail.len() > MAX_COMPLETION_STEP_DETAIL {
        return Err(SessionError::Oversized(format!(
            "ledger completion_step_status detail of {} bytes exceeds MAX_COMPLETION_STEP_DETAIL",
            detail.len()
        )));
    }
    if let Some(hash) = snapshot {
        let legacy = !hash.is_empty()
            && hash.len() <= MAX_VERIFICATION_TREE_HASH_BYTES
            && hash.bytes().all(|b| b.is_ascii_hexdigit());
        if !legacy && !faktor_fs::tree_manifest::is_tree_manifest_digest(hash) {
            return Err(SessionError::Malformed(
                "ledger completion_step_status snapshot must be a canonical `tm1:<64-hex>` \
                 tree-manifest digest or a non-empty legacy hex digest within \
                 MAX_VERIFICATION_TREE_HASH_BYTES"
                    .into(),
            ));
        }
    }
    if at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger completion_step_status at_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `integration_recorded` row, shared by the appender
/// and the strict decoder (a hostile raw row must fail loudly on read too).
/// Bounded fields are explicit: the stored file/conflict/source lists are
/// samples with exact counts and content digests beside them.
pub(crate) fn validate_integration_record(
    record: &IntegrationRecordRow,
) -> Result<(), SessionError> {
    let check_id = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.is_empty() || value.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record {what} must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
        Ok(())
    };
    let check_hex = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record {what} must be the 64-char hex BLAKE3"
            )));
        }
        Ok(())
    };
    check_id(&record.run_id, "run_id")?;
    if record.task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger integration_record task_id must be non-zero".into(),
        ));
    }
    if let Some(base) = &record.base_revision {
        check_id(base, "base_revision")?;
    }
    if let Some(base) = &record.base_snapshot {
        check_snapshot_digest(base, "base_snapshot")?;
    }
    // Explicit identity fields (hardening): each carries its own bound and
    // shape; the deprecated alias must AGREE with the explicit field when
    // both are present, so a tampered row can never name two base roots.
    if let Some(base) = &record.run_base_snapshot {
        check_snapshot_digest(base, "run_base_snapshot")?;
        if record
            .base_snapshot
            .as_deref()
            .is_some_and(|alias| alias != base)
        {
            return Err(SessionError::Malformed(
                "ledger integration_record base_snapshot and run_base_snapshot disagree".into(),
            ));
        }
    }
    if let Some(candidate) = &record.candidate_snapshot {
        check_snapshot_digest(candidate, "candidate_snapshot")?;
    }
    if let Some(landed) = &record.landed_snapshot {
        check_snapshot_digest(landed, "landed_snapshot")?;
        if !record.final_snapshot_hash.is_empty() && record.final_snapshot_hash != *landed {
            return Err(SessionError::Malformed(
                "ledger integration_record landed_snapshot and final_snapshot_hash disagree".into(),
            ));
        }
    }
    if let Some(digest) = &record.proof_basis_digest {
        if digest.is_empty() || digest.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record proof_basis_digest must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
    }
    if let Some(txn) = &record.integration_txn_id {
        if txn.is_empty() || txn.len() > MAX_INTEGRATION_ID_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record integration_txn_id must be 1..={MAX_INTEGRATION_ID_BYTES} bytes"
            )));
        }
    }
    if record.final_root.is_empty() || record.final_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(format!(
            "ledger integration_record final_root must be 1..={MAX_INTEGRATION_ROOT_BYTES} bytes"
        )));
    }
    // An EMPTY final hash is the record-first in-flight marker; a non-empty
    // one is the binding digest.
    if !record.final_snapshot_hash.is_empty() {
        check_snapshot_digest(&record.final_snapshot_hash, "final_snapshot_hash")?;
    }
    if record.integrated_files.len() > MAX_INTEGRATION_FILES {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} integrated files (cap {MAX_INTEGRATION_FILES})",
            record.integrated_files.len()
        )));
    }
    for path in &record.integrated_files {
        if path.is_empty() || path.len() > MAX_INTEGRATION_PATH_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record file path must be 1..={MAX_INTEGRATION_PATH_BYTES} bytes"
            )));
        }
    }
    if record.integrated_files.len() as u64 > record.integrated_file_count {
        return Err(SessionError::Malformed(
            "ledger integration_record stores more file rows than its file count".into(),
        ));
    }
    if record.integrated_file_count == 0 {
        if !record.integrated_files.is_empty() || !record.integrated_files_digest.is_empty() {
            return Err(SessionError::Malformed(
                "ledger integration_record with zero integrated files must carry no file rows or digest"
                    .into(),
            ));
        }
    } else {
        check_hex(&record.integrated_files_digest, "integrated_files_digest")?;
    }
    if record.conflicts.len() > MAX_INTEGRATION_CONFLICTS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} conflict rows (cap {MAX_INTEGRATION_CONFLICTS})",
            record.conflicts.len()
        )));
    }
    for conflict in &record.conflicts {
        if conflict.is_empty() || conflict.len() > MAX_INTEGRATION_CONFLICT_BYTES {
            return Err(SessionError::Malformed(format!(
                "ledger integration_record conflict row must be 1..={MAX_INTEGRATION_CONFLICT_BYTES} bytes"
            )));
        }
    }
    if record.conflicts.len() as u64 > record.conflict_count {
        return Err(SessionError::Malformed(
            "ledger integration_record stores more conflict rows than its conflict count".into(),
        ));
    }
    // A clean finalized record must carry a final snapshot; an in-flight one
    // may not pretend to have applied files. Conflicts are legal in both:
    // they can be recorded before the remaining children apply.
    if record.sources.len() > MAX_INTEGRATION_SOURCES {
        return Err(SessionError::Oversized(format!(
            "ledger integration_record stores {} sources (cap {MAX_INTEGRATION_SOURCES})",
            record.sources.len()
        )));
    }
    for source in &record.sources {
        check_id(&source.child_id, "source child_id")?;
        check_id(&source.change_set_id, "source change_set_id")?;
        check_snapshot_digest(&source.candidate_root_hash, "source candidate_root_hash")?;
    }
    if record.source_count < record.sources.len() as u64 {
        return Err(SessionError::Malformed(
            "ledger integration_record source count is smaller than its stored sources".into(),
        ));
    }
    if record.source_count == 0 {
        if !record.sources_digest.is_empty() {
            return Err(SessionError::Malformed(
                "ledger integration_record with zero sources must carry no sources digest".into(),
            ));
        }
    } else if record.sources.len() as u64 != record.source_count {
        // The bounded-full case: the digest covers every source.
        check_hex(&record.sources_digest, "sources_digest")?;
    }
    if record.at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger integration_record at_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `run_base` row, shared by the appender and the
/// strict decoder.
/// The verified-git artifact bound: a bounded manifest (entries × path
/// bytes), hex OIDs and a well-formed encoded remote ref.
const MAX_VERIFIED_GIT_MANIFEST_ENTRIES: usize = 200_000;
const MAX_VERIFIED_GIT_PATH_BYTES: usize = 4096;

fn is_hex_oid(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(crate) fn validate_verified_git_artifact(
    artifact: &VerifiedGitArtifact,
) -> Result<(), SessionError> {
    if artifact.task_id == 0 || artifact.revision == 0 || artifact.verification_record == 0 {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact ids must be non-zero".into(),
        ));
    }
    if artifact.verified_root_digest.is_empty()
        || artifact.verified_root_digest.len() > 128
        || !artifact
            .verified_root_digest
            .bytes()
            .all(|b| b.is_ascii_graphic())
    {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact verified_root_digest must be 1..=128 printable bytes"
                .into(),
        ));
    }
    if artifact.verified_manifest.is_empty()
        || artifact.verified_manifest.len() > MAX_VERIFIED_GIT_MANIFEST_ENTRIES
    {
        return Err(SessionError::Oversized(
            "ledger verified_git_artifact manifest must be 1..=MAX_VERIFIED_GIT_MANIFEST_ENTRIES entries"
                .into(),
        ));
    }
    let mut previous: Option<&str> = None;
    for entry in &artifact.verified_manifest {
        if entry.path.is_empty() || entry.path.len() > MAX_VERIFIED_GIT_PATH_BYTES {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact manifest path must be 1..=4096 bytes".into(),
            ));
        }
        let path = std::path::Path::new(&entry.path);
        if path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact manifest path must be plain and relative".into(),
            ));
        }
        if let Some(prev) = previous {
            if prev >= entry.path.as_str() {
                return Err(SessionError::Malformed(
                    "ledger verified_git_artifact manifest paths must be strictly sorted".into(),
                ));
            }
        }
        previous = Some(&entry.path);
    }
    for oid in [
        artifact.git_tree_oid.as_deref(),
        artifact.commit_oid.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !is_hex_oid(oid) {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact OID must be 40/64 hex chars".into(),
            ));
        }
    }
    if let Some(r) = artifact.local_ref.as_deref() {
        if !r.starts_with("refs/") || r.len() > 512 || r.contains(' ') {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact local_ref must be a well-formed ref name".into(),
            ));
        }
    }
    if let Some(remote) = artifact.remote_ref.as_deref() {
        // Encoded `<remote>:<refname>@<oid>`; both halves must be
        // well-formed and the oid is the exact commit the push reconciled.
        let (remote_name, rest) = remote.split_once(':').ok_or_else(|| {
            SessionError::Malformed(
                "ledger verified_git_artifact remote_ref must encode <remote>:<ref>@<oid>".into(),
            )
        })?;
        if remote_name.is_empty()
            || remote_name.len() > 256
            || remote_name.contains(|c: char| c.is_whitespace())
        {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote name must be well-formed".into(),
            ));
        }
        let (refname, oid) = rest.rsplit_once('@').ok_or_else(|| {
            SessionError::Malformed(
                "ledger verified_git_artifact remote_ref must encode <remote>:<ref>@<oid>".into(),
            )
        })?;
        if !refname.starts_with("refs/") || refname.len() > 512 || refname.contains(' ') {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote ref name must be well-formed".into(),
            ));
        }
        if !is_hex_oid(oid) {
            return Err(SessionError::Malformed(
                "ledger verified_git_artifact remote_ref oid must be a git oid".into(),
            ));
        }
    }
    if artifact.updated_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger verified_git_artifact updated_ms must be positive".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_run_base_record(record: &RunBaseRecord) -> Result<(), SessionError> {
    if record.run_id.is_empty() || record.run_id.len() > MAX_INTEGRATION_ID_BYTES {
        return Err(SessionError::Malformed(
            "ledger run_base run_id must be 1..=MAX_INTEGRATION_ID_BYTES".into(),
        ));
    }
    if record.workspace_id == 0 || record.worktree_id == 0 {
        return Err(SessionError::Malformed(
            "ledger run_base workspace_id/worktree_id must be non-zero".into(),
        ));
    }
    check_snapshot_digest(&record.snapshot_hash, "snapshot_hash")?;
    let manifest_legacy = record.manifest_digest.len() == MAX_RUN_BASE_DIGEST_BYTES
        && record
            .manifest_digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit());
    if !manifest_legacy {
        return Err(SessionError::Malformed(
            "ledger run_base manifest_digest must be the 64-char hex BLAKE3".into(),
        ));
    }
    if record.root.is_empty() || record.root.len() > MAX_RUN_BASE_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger run_base root must be 1..=MAX_RUN_BASE_ROOT_BYTES".into(),
        ));
    }
    if record.created_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger run_base created_ms must be positive".into(),
        ));
    }
    Ok(())
}

/// Shape bounds of one `integration_txn` row, shared by the appender and
/// the strict decoder.
pub(crate) fn validate_integration_txn(row: &IntegrationTxnRow) -> Result<(), SessionError> {
    let check_hex = |value: &str, what: &str| -> Result<(), SessionError> {
        if value.len() != MAX_RUN_BASE_DIGEST_BYTES || !value.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(SessionError::Malformed(format!(
                "ledger integration_txn {what} must be the 64-char hex BLAKE3"
            )));
        }
        Ok(())
    };
    if row.run_id.is_empty() || row.run_id.len() > MAX_INTEGRATION_ID_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn run_id must be 1..=MAX_INTEGRATION_ID_BYTES".into(),
        ));
    }
    if row.task_id == 0 {
        return Err(SessionError::Malformed(
            "ledger integration_txn task_id must be non-zero".into(),
        ));
    }
    if row.owner_root.is_empty() || row.owner_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn owner_root must be 1..=MAX_INTEGRATION_ROOT_BYTES".into(),
        ));
    }
    if row.candidate_root.is_empty() || row.candidate_root.len() > MAX_INTEGRATION_ROOT_BYTES {
        return Err(SessionError::Malformed(
            "ledger integration_txn candidate_root must be 1..=MAX_INTEGRATION_ROOT_BYTES".into(),
        ));
    }
    check_snapshot_digest(&row.run_base_snapshot, "run_base_snapshot")?;
    check_snapshot_digest(
        &row.verified_candidate_snapshot,
        "verified_candidate_snapshot",
    )?;
    if !row.sources_digest.is_empty() {
        check_hex(&row.sources_digest, "sources_digest")?;
    }
    if row.paths.len() > MAX_INTEGRATION_TXN_PATHS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_txn stores {} paths (cap {MAX_INTEGRATION_TXN_PATHS})",
            row.paths.len()
        )));
    }
    if row.path_count < row.paths.len() as u64 {
        return Err(SessionError::Malformed(
            "ledger integration_txn path count is smaller than its stored paths".into(),
        ));
    }
    if row.path_count == 0 && !row.paths.is_empty() {
        return Err(SessionError::Malformed(
            "ledger integration_txn with zero paths must carry no path rows".into(),
        ));
    }
    let mut previous: Option<&str> = None;
    for path in &row.paths {
        if path.path.is_empty() || path.path.len() > MAX_INTEGRATION_PATH_BYTES {
            return Err(SessionError::Malformed(
                "ledger integration_txn path must be 1..=MAX_INTEGRATION_PATH_BYTES".into(),
            ));
        }
        if previous.is_some_and(|p| p >= path.path.as_str()) {
            return Err(SessionError::Malformed(
                "ledger integration_txn paths must be strictly ascending".into(),
            ));
        }
        previous = Some(&path.path);
        // The canonical states and the rollback material must be internally
        // consistent on every NEW row: a regular base carries the CAS blob
        // of its exact payload digest, a symlink base its exact literal
        // target, an absent base no material at all. A legacy row
        // (`canonical == false`) is decoded additively and refused at
        // landing time, never validated as if it were authoritative.
        if path.canonical && !path.canonical_ready() {
            return Err(SessionError::Malformed(
                "ledger integration_txn path carries canonical states without their exact rollback material"
                    .into(),
            ));
        }
        if let Some(blob) = &path.rollback_blob {
            check_hex(blob, "path rollback_blob")?;
        }
        if let Some(target) = &path.rollback_link_target {
            if target.len() > faktor_fs::tree_manifest::MAX_TREE_MANIFEST_LINK_BYTES {
                return Err(SessionError::Oversized(
                    "ledger integration_txn symlink rollback target exceeds the manifest bound"
                        .into(),
                ));
            }
        }
    }
    if row.applied_count > row.path_count {
        return Err(SessionError::Malformed(
            "ledger integration_txn applied count exceeds its path count".into(),
        ));
    }
    if row.conflicts.len() > MAX_INTEGRATION_TXN_CONFLICTS {
        return Err(SessionError::Oversized(format!(
            "ledger integration_txn stores {} conflicts (cap {MAX_INTEGRATION_TXN_CONFLICTS})",
            row.conflicts.len()
        )));
    }
    for conflict in &row.conflicts {
        if conflict.is_empty() || conflict.len() > MAX_INTEGRATION_TXN_CONFLICT_BYTES {
            return Err(SessionError::Malformed(
                "ledger integration_txn conflict must be 1..=MAX_INTEGRATION_TXN_CONFLICT_BYTES"
                    .into(),
            ));
        }
    }
    if row.at_ms <= 0 {
        return Err(SessionError::Malformed(
            "ledger integration_txn at_ms must be positive".into(),
        ));
    }
    Ok(())
}

fn check_payload_bytes(payload: &LedgerPayload) -> Result<(), SessionError> {
    let bytes = json_bytes(&serde_json::to_value(payload).unwrap_or_default());
    if bytes > MAX_LEDGER_ENTRY_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger entry payload of {bytes} bytes exceeds MAX_LEDGER_ENTRY_BYTES"
        )));
    }
    Ok(())
}

fn malformed_row(what: &str, detail: &str) -> faktor_core::Error {
    SessionError::Malformed(format!("ledger {what} row is corrupt: {detail}")).into()
}

fn conflict_row(what: &str, detail: &str) -> faktor_core::Error {
    SessionError::Conflict(format!("ledger {what} row is inconsistent: {detail}")).into()
}

/// Shape bounds of one `edit_txn_prepared` row. Shared by the appender
/// (rejects BEFORE any byte is journaled) and the open-set reader (a
/// hostile raw-store row must fail there too).
fn validate_edit_txn_prepared(
    txn_id: u64,
    session: &str,
    files: &[EditTxnLedgerFile],
    strategy: &str,
) -> faktor_core::Result<()> {
    if txn_id == 0 {
        return Err(malformed_row(
            "edit_txn_prepared",
            "txn_id must be non-zero",
        ));
    }
    if session.is_empty() || session.len() > MAX_EDIT_TXN_SESSION_BYTES {
        return Err(malformed_row(
            "edit_txn_prepared",
            &format!("session must be 1..={MAX_EDIT_TXN_SESSION_BYTES} bytes"),
        ));
    }
    if files.is_empty() {
        return Err(malformed_row(
            "edit_txn_prepared",
            "at least one staged file is required",
        ));
    }
    if files.len() > MAX_EDIT_TXN_FILES {
        return Err(SessionError::Oversized(format!(
            "edit_txn_prepared of {} files exceeds MAX_EDIT_TXN_FILES",
            files.len()
        ))
        .into());
    }
    if !matches!(
        strategy,
        EDIT_TXN_STRATEGY_ROLL_FORWARD | EDIT_TXN_STRATEGY_ROLL_BACK
    ) {
        return Err(malformed_row(
            "edit_txn_prepared",
            &format!("strategy {strategy:?} is not roll_forward|roll_back"),
        ));
    }
    for f in files {
        check_text(&f.path, "edit txn file path")?;
        if faktor_core::hash::FileHash::from_hex(&f.base_digest).is_none() {
            return Err(malformed_row(
                "edit_txn_prepared",
                &format!(
                    "file {} has a base_digest that is not the 64-char hex BLAKE3",
                    f.path
                ),
            ));
        }
    }
    Ok(())
}

fn check_edit_txn_path_list(list: &[String], what: &str) -> Result<(), SessionError> {
    if list.len() > MAX_EDIT_TXN_TERMINAL_PATHS {
        return Err(SessionError::Oversized(format!(
            "ledger edit txn {what} of {} paths exceeds MAX_EDIT_TXN_TERMINAL_PATHS",
            list.len()
        )));
    }
    for p in list {
        check_text(p, what)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- head fold

fn fold(head: &mut LedgerHead, payload: &LedgerPayload) -> Result<(), SessionError> {
    match payload {
        LedgerPayload::GoalSet { goal } => {
            head.goal = goal.clone();
        }
        LedgerPayload::CriteriaSet {
            criteria,
            canonical,
        } => {
            head.criteria = criteria.clone();
            head.canonical_criteria = canonical.clone();
        }
        LedgerPayload::BlockerOpened { reason } => {
            if !head.open_blockers.iter().any(|r| r == reason) {
                head.open_blockers.push(reason.clone());
            }
        }
        LedgerPayload::BlockerResolved { reason } => {
            // Tolerant: resolving a blocker that is no longer open (its
            // opener was already pruned below the watermark) is a no-op,
            // never an error — the fold is a projection of what happened.
            head.open_blockers.retain(|r| r != reason);
        }
        LedgerPayload::Decision {
            step,
            choice,
            rationale,
        } => {
            let entry = LedgerDecision {
                step: step.clone(),
                choice: choice.clone(),
                rationale: rationale.clone(),
            };
            if head.decisions.first() != Some(&entry) {
                head.decisions.insert(0, entry);
                head.decisions.truncate(MAX_LEDGER_OPEN_BLOCKERS);
            }
        }
        LedgerPayload::PlanStepAdded {
            step_index,
            text,
            parent_index,
        } => {
            let row = LedgerPlanStep {
                step_index: *step_index,
                text: text.clone(),
                parent_index: *parent_index,
            };
            match head
                .plan_steps
                .iter_mut()
                .find(|p| p.step_index == *step_index)
            {
                Some(existing) => *existing = row,
                None => {
                    head.plan_steps.push(row);
                    head.plan_steps.sort_by_key(|p| p.step_index);
                }
            }
        }
        LedgerPayload::ChildAgentStarted {
            agent_id,
            task_id,
            worktree_id,
            purpose,
        } => {
            let row = LedgerChild {
                agent_id: *agent_id,
                task_id: *task_id,
                worktree_id: *worktree_id,
                purpose: purpose.clone(),
                outcome: None,
            };
            match head.children.iter_mut().find(|c| c.agent_id == *agent_id) {
                Some(existing) => *existing = row,
                None => head.children.push(row),
            }
        }
        LedgerPayload::ChildAgentFinished { agent_id, outcome } => {
            if let Some(child) = head
                .children
                .iter_mut()
                .find(|c| c.agent_id == *agent_id && c.outcome.is_none())
            {
                child.outcome = Some(outcome.clone());
            }
            // Tolerant when the running record was already pruned with its
            // start below the watermark (the fold is a projection).
        }
        LedgerPayload::RoutingDecision {
            turn,
            provider,
            model,
            reasoning,
            cost_micro,
        } => {
            head.routing_count = head.routing_count.saturating_add(1);
            head.routing_tail.push(LedgerRouting {
                turn: *turn,
                provider: provider.clone(),
                model: model.clone(),
                reasoning: reasoning.clone(),
                cost_micro: *cost_micro,
            });
            if head.routing_tail.len() > MAX_LEDGER_ROUTING_TAIL {
                head.routing_tail.remove(0);
            }
        }
        LedgerPayload::EpochBumped { from: _, to } => {
            head.epoch = Some(*to);
        }
        LedgerPayload::FailureRecorded { .. } => {}
        LedgerPayload::VerifyRun { checks, outcome } => {
            head.last_verify = Some(LedgerVerifySummary {
                checks: checks.clone(),
                outcome: outcome.clone(),
            });
        }
        LedgerPayload::TurnCompleted { .. } => {}
        // Learning records are corpus data, not head projections: they fold
        // nowhere and (unlike turn history) are pinned across compaction.
        LedgerPayload::LearningRecord { .. } => {}
        // Edit-transaction rows are OPERATIONAL state (crash recovery), not
        // head projections: they fold nowhere. Instead they are pinned in
        // the stream while open and compacted away once terminal, so the
        // never-lose contract keeps them exactly as long as recovery needs
        // them and prunes them exactly when it stops.
        LedgerPayload::EditTxnPrepared { .. }
        | LedgerPayload::EditTxnProgress { .. }
        | LedgerPayload::EditTxnCommitted { .. }
        | LedgerPayload::EditTxnRolledBack { .. } => {}
        // Tournament rows are the durable authority `Tournament::reopen`
        // folds them into a reconstructed tournament it owns: they fold
        // nowhere in the session head and are pinned in the stream (see
        // `compact_typed_ledger`).
        LedgerPayload::TournamentStarted { .. }
        | LedgerPayload::CandidateSettled { .. }
        | LedgerPayload::TournamentDecided { .. } => {}
        // Presentation rows fold the LATEST state per child into the head:
        // the head map is the durable presentation projection compaction
        // preserves, and a child absent from it is Foreground. The `from`
        // audit field is deliberately not cross-checked here — entries below
        // a compaction watermark may have been pruned into the head already,
        // and the sequence order alone (last wins) is the projection rule.
        LedgerPayload::ChildPresentationChanged { child_id, to, .. } => {
            head.presentations.insert(child_id.clone(), *to);
        }
        // Board rows project only their newest revision into the head (the
        // O(1) allocation/CAS cursor); the pinned rows themselves are the
        // authority the board reader reconstructs the live surface from.
        LedgerPayload::BoardPost { revision, .. } => {
            head.board_revision = head.board_revision.max(*revision);
        }
        LedgerPayload::BoardReset { new_revision, .. } => {
            head.board_revision = head.board_revision.max(*new_revision);
        }
        LedgerPayload::BoardRead { .. } | LedgerPayload::BoardReceipt { .. } => {}
        // Completion-contract rows are the durable authority the
        // `VerifiedComplete` gate reads (the accepted contract + the
        // per-step outcomes). They fold nowhere in the head and are pinned
        // across compaction, so the gate always evaluates the exact durable
        // history, never a lossy head projection.
        LedgerPayload::CompletionContractSet { .. }
        | LedgerPayload::CompletionStepStatus { .. } => {}
        // Integration records are the durable binding of a root verification
        // to the final integration snapshot: they fold nowhere in the head
        // and are pinned across compaction (the completion gate re-reads
        // them), exactly like tournament and completion-contract rows.
        LedgerPayload::IntegrationRecorded { .. } => {}
        // Run-base and landing-transaction rows are operational recovery
        // authorities: they fold nowhere in the head and are pinned across
        // compaction (staging and crash recovery re-read them).
        LedgerPayload::RunBaseRecorded { .. } | LedgerPayload::IntegrationTxnRecorded { .. } => {}
        // Verified-git publication artifacts are the durable authority the
        // commit/push/PR steps re-assert: they fold nowhere in the head and
        // are pinned across compaction.
        LedgerPayload::VerifiedGitArtifactRecorded { .. } => {}
        // External-operation rows are the write-before-call identity of an
        // unfinished external effect: they fold nowhere in the head and are
        // pinned across compaction (reconciliation re-reads the exact input
        // identity; a pruned row would turn a recorded effect into a guess).
        LedgerPayload::ExternalOperationRecorded { .. } => {}
        // Terminal-lifecycle rows are the ONE durable authority behind the
        // terminal service: they fold nowhere in the head and are pinned
        // across compaction (a recovery scan re-reads the stream).
        LedgerPayload::TerminalCreated { .. }
        | LedgerPayload::TerminalRunning { .. }
        | LedgerPayload::TerminalExited { .. }
        | LedgerPayload::TerminalKilled { .. }
        | LedgerPayload::TerminalLost { .. }
        | LedgerPayload::TerminalReconciled { .. } => {}
    }
    Ok(())
}

fn fold_entries(head: &mut LedgerHead, entries: &[TypedLedgerEntry]) -> Result<(), SessionError> {
    for entry in entries {
        fold(head, &entry.payload)?;
        head.checkpoint_seq = entry.seq;
    }
    Ok(())
}

fn head_to_json(head: &LedgerHead) -> Result<String, SessionError> {
    let json = serde_json::to_value(head)
        .map_err(|e| SessionError::Internal(format!("ledger head serialization: {e}")))?;
    let bytes = json_bytes(&json);
    if bytes > MAX_LEDGER_HEAD_BYTES {
        return Err(SessionError::Oversized(format!(
            "ledger head of {bytes} bytes exceeds MAX_LEDGER_HEAD_BYTES; \
             the ledger cannot be compacted to a larger head"
        )));
    }
    serde_json::to_string(&json)
        .map_err(|e| SessionError::Internal(format!("ledger head serialization: {e}")))
}

fn head_from_json(raw: &serde_json::Value) -> Result<LedgerHead, SessionError> {
    let head: LedgerHead = serde_json::from_value(raw.clone()).map_err(|e| {
        SessionError::Malformed(format!("ledger head JSON is corrupt (undecodable): {e}"))
    })?;
    if head.schema_ver != LEDGER_HEAD_SCHEMA_V {
        return Err(SessionError::Malformed(format!(
            "ledger head has unknown schema version {} (this reader understands v{LEDGER_HEAD_SCHEMA_V})",
            head.schema_ver
        )));
    }
    Ok(head)
}

// ---------------------------------------------------------------- handle API

impl SessionHandle {
    pub(crate) fn decode_row(
        &self,
        row: &faktor_store::LedgerEntryRow,
    ) -> Result<TypedLedgerEntry, SessionError> {
        decode_ledger_entry_row(row)
    }

    /// Read the session's typed ledger from durable rows, ascending by seq
    /// (bounded page; the caller pages with the cursor seq). Every row is
    /// decoded strictly: an unknown version or a shape violation is a loud
    /// error, never a silent drop.
    pub fn ledger_entries_page(
        &self,
        after_seq: Option<i64>,
        limit: u64,
    ) -> faktor_core::Result<LedgerEntryPage> {
        let limit = limit.clamp(1, MAX_LEDGER_PAGE);
        let rows = self
            .manager
            .store()
            .ledger_entries(self.id, after_seq, limit + 1)
            .map_err(map_store_err)?;
        let has_more = rows.len() as u64 > limit;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows.into_iter().take(limit as usize) {
            entries.push(self.decode_row(&row)?);
        }
        Ok(LedgerEntryPage { entries, has_more })
    }

    fn all_entries_decoded(&self) -> Result<Vec<TypedLedgerEntry>, SessionError> {
        let store = self.manager.store();
        let mut out = Vec::new();
        let mut cursor: Option<i64> = None;
        loop {
            let page = store
                .ledger_entries(self.id, cursor, 1000)
                .map_err(map_store_err)?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|r| r.seq);
            for row in page {
                out.push(self.decode_row(&row)?);
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Read every durable learning-crate record of this session, ascending
    /// by seq, each decoded STRICTLY (an unknown kind, an unknown schema
    /// version or a shape violation is a loud error — never a silent drop).
    /// Additive read seam for the `faktor-learning` durable store adapter;
    /// rows of other ledger kinds are skipped. Memory is bounded by the
    /// session's own learning rows (the adapter caps how many it appends).
    pub fn ledger_learning_records(&self) -> faktor_core::Result<Vec<LearningRecordRow>> {
        let mut out = Vec::new();
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::LearningRecord { record, payload } = entry.payload {
                out.push(LearningRecordRow {
                    seq: entry.seq,
                    record,
                    payload,
                });
            }
        }
        Ok(out)
    }

    /// Fold every entry after the materialized head's checkpoint onto the
    /// head (crash recovery between an entry append and its head
    /// checkpoint) and persist the refreshed checkpoint. A missing or
    /// undecodable head is rebuilt by replaying the surviving entries
    /// (entries are the authority). An ENTRY that fails its schema decode
    /// is a loud error — the ledger never silently drops one.
    pub fn ledger_ensure_head(&self) -> faktor_core::Result<LedgerHead> {
        let store = self.manager.store();
        let max_seq = store.ledger_max_seq(self.id).map_err(map_store_err)?;
        // A ledger_head row whose JSON is undecodable is a hand-corrupted
        // head: recover by replay from entries (never fail the open).
        let head_row = store
            .ledger_head(self.id)
            .map_err(map_store_err)
            .unwrap_or_default();
        let mut head: LedgerHead = LedgerHead {
            schema_ver: LEDGER_HEAD_SCHEMA_V,
            ..Default::default()
        };
        let mut after: Option<i64> = None;
        if let Some(row) = &head_row {
            if row.schema_ver != LEDGER_HEAD_SCHEMA_V {
                // Unknown FUTURE head schema: silently rebuilding would
                // discard head-only content compaction folded; refuse loudly.
                return Err(SessionError::Malformed(format!(
                    "ledger head has unknown schema version {}; refusing to misread it \
                     (delete the head row to force a rebuild from entries)",
                    row.schema_ver
                ))
                .into());
            }
            match head_from_json(&row.head_json) {
                Ok(h) => {
                    head = h;
                    after = Some(head.checkpoint_seq);
                }
                // Corrupt head JSON: replay the surviving entries from
                // scratch (the rebuilt checkpoint may lose content that
                // compaction had pruned below the entries — the entries
                // are the authority on what survived).
                Err(SessionError::Malformed(_)) => after = None,
                Err(e) => return Err(e.into()),
            }
        }
        let stale = match after {
            Some(checkpoint) => checkpoint < max_seq,
            None => max_seq > 0 || head_row.is_some(),
        };
        if !stale {
            return Ok(head);
        }
        let rows = store
            .ledger_entries(self.id, after, u64::MAX)
            .map_err(map_store_err)?;
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(self.decode_row(&row)?);
        }
        fold_entries(&mut head, &entries)?;
        let json = head_to_json(&head)?;
        let head_value = serde_json::from_str(&json)
            .map_err(|e| SessionError::Internal(format!("head json: {e}")))?;
        store
            .put_ledger_head(
                self.id,
                head_value,
                head.checkpoint_seq,
                LEDGER_HEAD_SCHEMA_V,
            )
            .map_err(map_store_err)?;
        Ok(head)
    }

    // ---------------------------------------------------- typed appenders

    /// Append `GoalSet {goal}` (bounded; a fresh goal replaces the old in
    /// the projection).
    pub fn ledger_goal_set(&self, goal: &str) -> faktor_core::Result<Option<i64>> {
        check_text(goal, "goal")?;
        self.append_entry(LedgerPayload::GoalSet {
            goal: goal.to_string(),
        })
    }

    /// Append `CriteriaSet` with the wave-8 canonical criteria: the derived
    /// row list and its canonical joined text.
    pub fn ledger_criteria_set(
        &self,
        criteria: &[String],
        canonical: &str,
    ) -> faktor_core::Result<Option<i64>> {
        if criteria.is_empty() {
            return Err(SessionError::Malformed(
                "criteria_set requires at least one criterion".into(),
            )
            .into());
        }
        if criteria.len() > MAX_LEDGER_CRITERIA {
            return Err(SessionError::Oversized(format!(
                "{} criteria exceed MAX_LEDGER_CRITERIA",
                criteria.len()
            ))
            .into());
        }
        for c in criteria {
            check_text(c, "criterion")?;
        }
        if canonical.is_empty() || canonical.len() > MAX_LEDGER_TEXT * 4 {
            return Err(SessionError::Malformed(
                "criteria canonical text must be 1..=16384 bytes".into(),
            )
            .into());
        }
        self.append_entry(LedgerPayload::CriteriaSet {
            criteria: criteria.to_vec(),
            canonical: canonical.to_string(),
        })
    }

    /// Append `BlockerOpened {reason}`. A reason that is already open is a
    /// no-op (Ok(None)) — the gate is open, not re-opened. Opening beyond
    /// [`MAX_LEDGER_OPEN_BLOCKERS`] is a loud error.
    pub fn ledger_blocker_opened(&self, reason: &str) -> faktor_core::Result<Option<i64>> {
        check_text(reason, "blocker reason")?;
        let head = self.ledger_ensure_head()?;
        if head.open_blockers.iter().any(|r| r == reason) {
            return Ok(None);
        }
        if head.open_blockers.len() >= MAX_LEDGER_OPEN_BLOCKERS {
            return Err(SessionError::Oversized(format!(
                "{} open blockers exceed MAX_LEDGER_OPEN_BLOCKERS; resolve blockers before opening more",
                head.open_blockers.len()
            ))
            .into());
        }
        self.append_entry(LedgerPayload::BlockerOpened {
            reason: reason.to_string(),
        })
    }

    /// Append `BlockerResolved {reason}`. A typed error when the reason is
    /// not open (nothing to resolve) — loud, never a silent no-op.
    pub fn ledger_blocker_resolved(&self, reason: &str) -> faktor_core::Result<Option<i64>> {
        check_text(reason, "blocker reason")?;
        let head = self.ledger_ensure_head()?;
        if !head.open_blockers.iter().any(|r| r == reason) {
            return Err(SessionError::Conflict(format!(
                "blocker {reason:?} is not open; nothing to resolve"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::BlockerResolved {
            reason: reason.to_string(),
        })
    }

    /// Typed delegation over [`Self::ledger_blocker_opened`]: the shared
    /// [`faktor_core::blocker::ChildBlocker`] is validated first (bounded
    /// text, closed kind) and its stable reason string rides the EXISTING
    /// `BlockerOpened` payload — same durable row, same fold, no schema
    /// change (additive compat with the historic string API).
    pub fn ledger_child_blocker_opened(
        &self,
        blocker: &faktor_core::blocker::ChildBlocker,
    ) -> faktor_core::Result<Option<i64>> {
        blocker.validate()?;
        self.ledger_blocker_opened(&blocker.ledger_reason())
    }

    /// Typed delegation over [`Self::ledger_blocker_resolved`] (same compat
    /// contract as [`Self::ledger_child_blocker_opened`]).
    pub fn ledger_child_blocker_resolved(
        &self,
        blocker: &faktor_core::blocker::ChildBlocker,
    ) -> faktor_core::Result<Option<i64>> {
        blocker.validate()?;
        self.ledger_blocker_resolved(&blocker.ledger_reason())
    }

    /// Append one `Decision {step, choice, rationale}`.
    pub fn ledger_decision(
        &self,
        step: &str,
        choice: &str,
        rationale: &str,
    ) -> faktor_core::Result<Option<i64>> {
        check_text(step, "decision step")?;
        check_text(choice, "decision choice")?;
        check_text(rationale, "decision rationale")?;
        self.append_entry(LedgerPayload::Decision {
            step: step.to_string(),
            choice: choice.to_string(),
            rationale: rationale.to_string(),
        })
    }

    /// Mirror one plan step as `PlanStepAdded {step_index, text,
    /// parent_index}` (`parent_index` = the prior step it extends). The
    /// mirror is idempotent: re-recording an index replaces it.
    pub fn ledger_plan_step_added(
        &self,
        step_index: u32,
        text: &str,
        parent_index: Option<u32>,
    ) -> faktor_core::Result<Option<i64>> {
        check_text(text, "plan step")?;
        if step_index >= MAX_LEDGER_PLAN_STEPS {
            return Err(SessionError::Oversized(format!(
                "plan step index {step_index} exceeds MAX_LEDGER_PLAN_STEPS"
            ))
            .into());
        }
        if parent_index.is_some_and(|p| p >= step_index) {
            return Err(SessionError::Malformed(format!(
                "plan step {step_index} cannot have parent {parent_index:?} (parents precede their step)"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::PlanStepAdded {
            step_index,
            text: text.to_string(),
            parent_index,
        })
    }

    /// Record a child agent start. A second start of the same RUNNING
    /// agent is a typed error; a finished agent may start again.
    pub fn ledger_child_started(
        &self,
        agent_id: u64,
        task_id: u64,
        worktree_id: u64,
        purpose: &str,
    ) -> faktor_core::Result<Option<i64>> {
        if agent_id == 0 || task_id == 0 || worktree_id == 0 {
            return Err(SessionError::Malformed("child agent ids must be non-zero".into()).into());
        }
        check_text(purpose, "child purpose")?;
        let head = self.ledger_ensure_head()?;
        if head
            .children
            .iter()
            .any(|c| c.agent_id == agent_id && c.outcome.is_none())
        {
            return Err(SessionError::Conflict(format!(
                "child agent {agent_id} is already running"
            ))
            .into());
        }
        if head.children.len() >= MAX_LEDGER_CHILDREN {
            return Err(SessionError::Oversized(format!(
                "{} child records exceed MAX_LEDGER_CHILDREN",
                head.children.len()
            ))
            .into());
        }
        self.append_entry(LedgerPayload::ChildAgentStarted {
            agent_id,
            task_id,
            worktree_id,
            purpose: purpose.to_string(),
        })
    }

    /// Record a child agent finish. A finish WITHOUT a running start is a
    /// typed error at append time (out-of-order stream is corruption).
    pub fn ledger_child_finished(
        &self,
        agent_id: u64,
        outcome: &str,
    ) -> faktor_core::Result<Option<i64>> {
        check_text(outcome, "child outcome")?;
        let head = self.ledger_ensure_head()?;
        if !head
            .children
            .iter()
            .any(|c| c.agent_id == agent_id && c.outcome.is_none())
        {
            return Err(SessionError::Malformed(format!(
                "child agent finish without a running start: agent {agent_id} has no open start"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::ChildAgentFinished {
            agent_id,
            outcome: outcome.to_string(),
        })
    }

    /// Record one routing decision (`turn` = the op id of the routed
    /// logical turn), with the router's reasoning and estimated cost.
    pub fn ledger_routing_decision(
        &self,
        turn: u64,
        provider: &str,
        model: &str,
        reasoning: &str,
        cost_micro: u64,
    ) -> faktor_core::Result<Option<i64>> {
        if provider.is_empty() || provider.len() > 256 {
            return Err(SessionError::Malformed("provider must be 1..=256 bytes".into()).into());
        }
        if model.is_empty() || model.len() > 256 {
            return Err(SessionError::Malformed("model must be 1..=256 bytes".into()).into());
        }
        if reasoning.len() > MAX_LEDGER_TEXT {
            return Err(SessionError::Oversized(format!(
                "routing reasoning of {} bytes exceeds {MAX_LEDGER_TEXT}",
                reasoning.len()
            ))
            .into());
        }
        self.append_entry(LedgerPayload::RoutingDecision {
            turn,
            provider: provider.to_string(),
            model: model.to_string(),
            reasoning: reasoning.to_string(),
            cost_micro,
        })
    }

    /// Record an instruction-epoch bump `from -> to` (`from` = the epoch
    /// recorded in the head, None when nothing was recorded yet). A no-op
    /// when the head already records `to`.
    pub fn ledger_epoch_bumped(
        &self,
        from: Option<u64>,
        to: u64,
    ) -> faktor_core::Result<Option<i64>> {
        if from == Some(to) {
            return Err(SessionError::Malformed("epoch_bumped requires from != to".into()).into());
        }
        let head = self.ledger_ensure_head()?;
        if head.epoch == Some(to) {
            return Ok(None);
        }
        let from = from.or(head.epoch);
        self.append_entry(LedgerPayload::EpochBumped { from, to })
    }

    /// Record one turn failure (bounded; mirrors the ledger blob's
    /// known-failures list).
    pub fn ledger_failure_recorded(&self, failure: &str) -> faktor_core::Result<Option<i64>> {
        check_text(failure, "failure")?;
        self.append_entry(LedgerPayload::FailureRecorded {
            failure: failure.to_string(),
        })
    }

    /// Record one genuine end-of-turn verification run. `outcome` is one of
    /// `passed|failed|blocked|pending|unverified`; a typo is a typed error.
    pub fn ledger_verify_run(
        &self,
        checks: &[LedgerCheckRun],
        outcome: &str,
    ) -> faktor_core::Result<Option<i64>> {
        if checks.is_empty() {
            return Err(SessionError::Malformed(
                "verify_run requires at least one executed check".into(),
            )
            .into());
        }
        if checks.len() > 64 {
            return Err(SessionError::Oversized("too many checks in one verify_run".into()).into());
        }
        for c in checks {
            if c.id.is_empty() || c.id.len() > 128 {
                return Err(
                    SessionError::Malformed("check id must be 1..=128 bytes".into()).into(),
                );
            }
        }
        if !matches!(
            outcome,
            "passed" | "failed" | "blocked" | "pending" | "unverified"
        ) {
            return Err(SessionError::Malformed(format!(
                "verify_run outcome {outcome:?} is not one of passed|failed|blocked|pending|unverified"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::VerifyRun {
            checks: checks.to_vec(),
            outcome: outcome.to_string(),
        })
    }

    /// Record one genuine logical-turn completion.
    pub fn ledger_turn_completed(&self, turn: u64) -> faktor_core::Result<Option<i64>> {
        if turn == 0 {
            return Err(SessionError::Malformed(
                "turn_completed requires a non-zero turn id".into(),
            )
            .into());
        }
        self.append_entry(LedgerPayload::TurnCompleted { turn })
    }

    /// Append one durable learning-crate record (audits 65-67/92): `record`
    /// is `episode|learning|removed`, `payload` the learning crate's bounded
    /// JSON document (schema owned by `faktor-learning`, stored verbatim).
    /// Shape bounds are enforced BEFORE any byte is journaled.
    pub fn ledger_learning_record(
        &self,
        record: &str,
        payload: &str,
    ) -> faktor_core::Result<Option<i64>> {
        validate_learning_record(record, payload)?;
        self.append_entry(LedgerPayload::LearningRecord {
            record: record.to_string(),
            payload: payload.to_string(),
        })
    }

    // ------------------------------------------ durable tournament entries

    /// Record that one implementation tournament started: the goal every
    /// candidate received, the byte-identical criteria and the N candidate
    /// seeds. Shape bounds are enforced before any byte is journaled.
    pub fn ledger_tournament_started(
        &self,
        tournament_id: &str,
        run_family: &str,
        goal: &str,
        criteria: &[TournamentCriterionRow],
        candidates: &[TournamentCandidateRow],
    ) -> faktor_core::Result<Option<i64>> {
        validate_tournament_started(tournament_id, run_family, goal, criteria, candidates)?;
        self.append_entry(LedgerPayload::TournamentStarted {
            tournament_id: tournament_id.to_string(),
            run_family: run_family.to_string(),
            goal: goal.to_string(),
            criteria: criteria.to_vec(),
            candidates: candidates.to_vec(),
        })
    }

    /// Record one settled candidate of a tournament (verification +
    /// independent review + cost/wall evidence). Shape bounds only —
    /// cross-row consistency (known tournament, one settlement per
    /// candidate, derived-spec equality) is enforced by the orchestrator's
    /// `Tournament::reopen`, never silently accepted.
    pub fn ledger_candidate_settled(
        &self,
        tournament_id: &str,
        settlement: &TournamentSettlementRow,
    ) -> faktor_core::Result<Option<i64>> {
        validate_tournament_id(tournament_id, "tournament id")?;
        validate_tournament_settlement(settlement)?;
        self.append_entry(LedgerPayload::CandidateSettled {
            tournament_id: tournament_id.to_string(),
            settlement: settlement.clone(),
        })
    }

    /// Record the terminal row of one tournament: `outcome` is
    /// `decided|aborted`, `winner` names the candidate proposed for the
    /// explicit approved-merge path (None on abort). The bounded outcome
    /// text is the audit rationale (why the winner won, why losers were
    /// discarded).
    pub fn ledger_tournament_decided(
        &self,
        tournament_id: &str,
        winner: Option<&str>,
        outcome: &str,
        rationale: &str,
    ) -> faktor_core::Result<Option<i64>> {
        validate_tournament_id(tournament_id, "tournament id")?;
        if let Some(w) = winner {
            validate_tournament_text(w, "tournament winner")?;
        }
        if !matches!(
            outcome,
            TOURNAMENT_OUTCOME_DECIDED | TOURNAMENT_OUTCOME_ABORTED
        ) {
            return Err(SessionError::Malformed(format!(
                "ledger tournament_decided outcome {outcome:?} is not decided|aborted"
            ))
            .into());
        }
        if outcome == TOURNAMENT_OUTCOME_DECIDED && winner.is_none() {
            return Err(SessionError::Malformed(
                "ledger tournament_decided with outcome decided requires a winner".into(),
            )
            .into());
        }
        if rationale.is_empty() || rationale.len() > MAX_TOURNAMENT_OUTCOME {
            return Err(SessionError::Malformed(
                "ledger tournament_decided rationale must be 1..=MAX_TOURNAMENT_OUTCOME bytes"
                    .into(),
            )
            .into());
        }
        self.append_entry(LedgerPayload::TournamentDecided {
            tournament_id: tournament_id.to_string(),
            winner: winner.map(str::to_string),
            outcome: outcome.to_string(),
            rationale: rationale.to_string(),
        })
    }

    // ------------------------------------------ durable edit txn entries (P0-53)

    /// Record that a multi-file edit transaction was durably PREPARED
    /// (record-first: every file staged and validated, nothing written
    /// yet). Shape bounds are enforced here before any byte is journaled;
    /// cross-row semantic consistency (duplicate prepares, orphan or
    /// out-of-range progress) is validated when the OPEN set is read by
    /// [`SessionHandle::ledger_open_edit_txns`] — never silently accepted.
    pub fn ledger_edit_txn_prepared(
        &self,
        txn_id: u64,
        session: &str,
        files: &[EditTxnLedgerFile],
        strategy: &str,
    ) -> faktor_core::Result<Option<i64>> {
        validate_edit_txn_prepared(txn_id, session, files, strategy)?;
        self.append_entry(LedgerPayload::EditTxnPrepared {
            txn_id,
            session: session.to_string(),
            files: files.to_vec(),
            strategy: strategy.to_string(),
        })
    }

    /// Record one per-file outcome of an open edit transaction. Journaled
    /// AFTER the file's commit-time CAS: `outcome` is `committed` or
    /// `conflicted`. Shape bounds only — the row's consistency with its
    /// prepared transaction is checked when the open set is read.
    pub fn ledger_edit_txn_progress(
        &self,
        txn_id: u64,
        seq: u64,
        path: &str,
        outcome: &str,
    ) -> faktor_core::Result<Option<i64>> {
        if txn_id == 0 {
            return Err(SessionError::Malformed(
                "edit_txn_progress requires a non-zero txn_id".into(),
            )
            .into());
        }
        check_text(path, "edit txn progress path")?;
        if !matches!(
            outcome,
            EDIT_TXN_OUTCOME_COMMITTED | EDIT_TXN_OUTCOME_CONFLICTED
        ) {
            return Err(SessionError::Malformed(format!(
                "edit_txn_progress outcome {outcome:?} is not committed|conflicted"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::EditTxnProgress {
            txn_id,
            seq,
            path: path.to_string(),
            outcome: outcome.to_string(),
        })
    }

    /// Record the terminal row of an edit transaction that finished WITHOUT
    /// a rollback (all committed, or a `roll_forward` conflict stop).
    pub fn ledger_edit_txn_committed(
        &self,
        txn_id: u64,
        committed: &[String],
        conflicted: &[String],
        skipped: &[String],
    ) -> faktor_core::Result<Option<i64>> {
        if txn_id == 0 {
            return Err(SessionError::Malformed(
                "edit_txn_committed requires a non-zero txn_id".into(),
            )
            .into());
        }
        for (what, list) in [
            ("committed", committed),
            ("conflicted", conflicted),
            ("skipped", skipped),
        ] {
            check_edit_txn_path_list(list, what)?;
        }
        self.append_entry(LedgerPayload::EditTxnCommitted {
            txn_id,
            committed: committed.to_vec(),
            conflicted: conflicted.to_vec(),
            skipped: skipped.to_vec(),
        })
    }

    /// Record the terminal row of a `roll_back` transaction that hit a
    /// conflict: the already-committed files were CAS-restored, or refused
    /// and listed in `rollback_conflicts` (never clobbered).
    pub fn ledger_edit_txn_rolled_back(
        &self,
        txn_id: u64,
        rolled_back: &[String],
        rollback_conflicts: &[String],
    ) -> faktor_core::Result<Option<i64>> {
        if txn_id == 0 {
            return Err(SessionError::Malformed(
                "edit_txn_rolled_back requires a non-zero txn_id".into(),
            )
            .into());
        }
        for (what, list) in [
            ("rolled_back", rolled_back),
            ("rollback_conflicts", rollback_conflicts),
        ] {
            check_edit_txn_path_list(list, what)?;
        }
        self.append_entry(LedgerPayload::EditTxnRolledBack {
            txn_id,
            rolled_back: rolled_back.to_vec(),
            rollback_conflicts: rollback_conflicts.to_vec(),
        })
    }

    /// The OPEN durable edit transactions of this session: every
    /// `edit_txn_prepared` row without a matching terminal
    /// (`edit_txn_committed`/`edit_txn_rolled_back`) row, plus its decoded
    /// progress rows — the exact input crash recovery replays.
    ///
    /// Validation is strict and loud (this is a read of DURABLE recovery
    /// state, so hostile raw-store rows must never pass silently):
    ///
    /// - a duplicate `edit_txn_prepared` of one txn id (a re-execution must
    ///   recover, never re-begin),
    /// - a progress/terminal row without a preceding prepared row (orphan),
    /// - a progress row after its terminal, a prepared row after progress or
    ///   a second terminal (out-of-order stream),
    /// - an out-of-range progress seq, a path that does not match the
    ///   prepared file at that seq, or a duplicate seq,
    /// - any payload field violation.
    ///
    /// All of the above are TYPED errors and NO open set is returned: the
    /// session stays open, the affected transaction stays open (never
    /// silently dropped), and recovery makes zero writes.
    pub fn ledger_open_edit_txns(&self) -> faktor_core::Result<Vec<EditTxnOpenRow>> {
        let entries = self.all_entries_decoded()?;
        // One fold over the stream, ascending by seq; per-txn phase machine:
        // None -> (prepared) -> Prepared -> (progress* | terminal) ->
        // Terminal. Validation lives HERE, never at append time (progress
        // appends run once per committed file and must not rescan).
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Phase {
            Prepared,
            Terminal,
        }
        let mut phase: BTreeMap<u64, Phase> = BTreeMap::new();
        let mut prepared: BTreeMap<u64, (Vec<EditTxnLedgerFile>, String, String)> = BTreeMap::new();
        struct ProgressRow {
            txn_id: u64,
            seq: u64,
            path: String,
            outcome: String,
        }
        let mut progress: Vec<ProgressRow> = Vec::new();
        for entry in &entries {
            match &entry.payload {
                LedgerPayload::EditTxnPrepared {
                    txn_id,
                    session,
                    files,
                    strategy,
                } => {
                    validate_edit_txn_prepared(*txn_id, session, files, strategy)?;
                    if phase.insert(*txn_id, Phase::Prepared).is_some() {
                        return Err(malformed_row(
                            "edit_txn_prepared",
                            &format!(
                                "duplicate prepared rows for txn {txn_id}; a re-execution must recover, never re-begin"
                            ),
                        ));
                    }
                    prepared.insert(*txn_id, (files.clone(), session.clone(), strategy.clone()));
                }
                LedgerPayload::EditTxnProgress {
                    txn_id,
                    seq,
                    path,
                    outcome,
                } => {
                    if *txn_id == 0 {
                        return Err(malformed_row(
                            "edit_txn_progress",
                            "txn_id must be non-zero",
                        ));
                    }
                    check_text(path, "edit txn progress path")?;
                    if !matches!(
                        outcome.as_str(),
                        EDIT_TXN_OUTCOME_COMMITTED | EDIT_TXN_OUTCOME_CONFLICTED
                    ) {
                        return Err(malformed_row(
                            "edit_txn_progress",
                            &format!("outcome {outcome:?} is not committed|conflicted"),
                        ));
                    }
                    if phase.get(txn_id) != Some(&Phase::Prepared) {
                        // Orphan (no prepared yet) or late (after terminal).
                        return Err(malformed_row(
                            "edit_txn_progress",
                            &format!("progress row of txn {txn_id} is orphaned or out of order"),
                        ));
                    }
                    progress.push(ProgressRow {
                        txn_id: *txn_id,
                        seq: *seq,
                        path: path.clone(),
                        outcome: outcome.clone(),
                    });
                }
                LedgerPayload::EditTxnCommitted {
                    txn_id,
                    committed,
                    conflicted,
                    skipped,
                } => {
                    for (what, list) in [
                        ("committed", committed),
                        ("conflicted", conflicted),
                        ("skipped", skipped),
                    ] {
                        check_edit_txn_path_list(list, what)?;
                    }
                    if !prepared.contains_key(txn_id) {
                        return Err(conflict_row(
                            "edit_txn_committed",
                            &format!("terminal row references unknown edit txn {txn_id}"),
                        ));
                    }
                    if phase.get(txn_id) == Some(&Phase::Terminal) {
                        return Err(malformed_row(
                            "edit_txn_committed",
                            &format!("duplicate terminal row of txn {txn_id}"),
                        ));
                    }
                    phase.insert(*txn_id, Phase::Terminal);
                }
                LedgerPayload::EditTxnRolledBack {
                    txn_id,
                    rolled_back,
                    rollback_conflicts,
                } => {
                    for (what, list) in [
                        ("rolled_back", rolled_back),
                        ("rollback_conflicts", rollback_conflicts),
                    ] {
                        check_edit_txn_path_list(list, what)?;
                    }
                    if !prepared.contains_key(txn_id) {
                        return Err(conflict_row(
                            "edit_txn_rolled_back",
                            &format!("terminal row references unknown edit txn {txn_id}"),
                        ));
                    }
                    if phase.get(txn_id) == Some(&Phase::Terminal) {
                        return Err(malformed_row(
                            "edit_txn_rolled_back",
                            &format!("duplicate terminal row of txn {txn_id}"),
                        ));
                    }
                    phase.insert(*txn_id, Phase::Terminal);
                }
                _ => {}
            }
        }
        // Assemble the OPEN transactions (prepared, never terminaled).
        let mut out: Vec<EditTxnOpenRow> = Vec::new();
        for (txn_id, (files, session, strategy)) in &prepared {
            if phase.get(txn_id) == Some(&Phase::Terminal) {
                continue;
            }
            let mut progress_rows: Vec<EditTxnOpenProgress> = Vec::new();
            for row in progress.iter().filter(|r| &r.txn_id == txn_id) {
                let idx = usize::try_from(row.seq).map_err(|_| {
                    malformed_row("edit_txn_progress", &format!("seq {} overflows", row.seq))
                })?;
                if idx >= files.len() {
                    return Err(malformed_row(
                        "edit_txn_progress",
                        &format!(
                            "seq {} of txn {txn_id} is out of range ({} files prepared)",
                            row.seq,
                            files.len()
                        ),
                    ));
                }
                if files[idx].path != row.path {
                    return Err(malformed_row(
                        "edit_txn_progress",
                        &format!(
                            "path {:?} of txn {txn_id} seq {} does not match prepared file {:?}",
                            row.path, row.seq, files[idx].path
                        ),
                    ));
                }
                progress_rows.push(EditTxnOpenProgress {
                    seq: row.seq,
                    path: row.path.clone(),
                    outcome: row.outcome.clone(),
                });
            }
            progress_rows.sort_by_key(|r| r.seq);
            if let Some(w) = progress_rows.windows(2).find(|w| w[0].seq == w[1].seq) {
                return Err(malformed_row(
                    "edit_txn_progress",
                    &format!("duplicate progress seq {} for txn {txn_id}", w[0].seq),
                ));
            }
            out.push(EditTxnOpenRow {
                txn_id: *txn_id,
                session: session.clone(),
                strategy: strategy.clone(),
                files: files.clone(),
                progress: progress_rows,
            });
        }
        out.sort_by_key(|r| r.txn_id);
        Ok(out)
    }

    // ---------------------------------------------------- completion contract

    /// Record the accepted PR/CI-fix completion contract of ONE task run
    /// (`task_id`, the task `revision` the run started at). Immutable per
    /// revision: a second set for the same `(task_id, revision)` is a typed
    /// Conflict, checked and appended under the session command lock so two
    /// racing writers cannot both win. An all-false contract is refused (the
    /// default path carries no row).
    pub fn ledger_completion_contract_set(
        &self,
        task_id: u64,
        revision: u64,
        contract: &CompletionContract,
    ) -> faktor_core::Result<Option<i64>> {
        validate_completion_contract_set(task_id, revision, contract)?;
        let _guard = self.command_guard();
        if self
            .ledger_completion_contract_at(task_id, revision)?
            .is_some()
        {
            return Err(SessionError::Conflict(format!(
                "completion contract for task {task_id} revision {revision} is already recorded; \
                 a contract is immutable per task revision"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::CompletionContractSet {
            task_id,
            revision,
            contract: *contract,
        })
    }

    /// The task's ACCEPTED contract: the newest `completion_contract_set`
    /// row for `task_id` (contracts are keyed by the run's start revision;
    /// a later run on a later revision supersedes an earlier one). `None`
    /// when the task never carried a non-default contract.
    pub fn ledger_completion_contract(
        &self,
        task_id: u64,
    ) -> faktor_core::Result<Option<CompletionContractRow>> {
        let mut latest: Option<CompletionContractRow> = None;
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::CompletionContractSet {
                task_id: row_task,
                revision,
                contract,
            } = entry.payload
            {
                if row_task == task_id {
                    // Entries ascend by seq: the last match wins.
                    latest = Some(CompletionContractRow {
                        seq: entry.seq,
                        task_id: row_task,
                        revision,
                        contract,
                    });
                }
            }
        }
        Ok(latest)
    }

    /// [`Self::ledger_completion_contract`] with the explicit read
    /// classification: a corrupt entry stream is `PresentMalformed` (never
    /// "no contract"), a failed read is `StoreFailure`, and only a genuinely
    /// absent contract is `Missing`.
    pub fn ledger_completion_contract_read(
        &self,
        task_id: u64,
    ) -> DurableRead<CompletionContractRow> {
        let mut latest: Option<CompletionContractRow> = None;
        let entries = match self.all_entries_decoded() {
            Ok(entries) => entries,
            Err(e) => return DurableRead::from_session_error(e),
        };
        for entry in entries {
            if let LedgerPayload::CompletionContractSet {
                task_id: row_task,
                revision,
                contract,
            } = entry.payload
            {
                if row_task == task_id {
                    // Entries ascend by seq: the last match wins.
                    latest = Some(CompletionContractRow {
                        seq: entry.seq,
                        task_id: row_task,
                        revision,
                        contract,
                    });
                }
            }
        }
        match latest {
            Some(row) => DurableRead::PresentValid(row),
            None => DurableRead::Missing,
        }
    }

    /// The exact contract row of one `(task_id, revision)`, if any.
    pub fn ledger_completion_contract_at(
        &self,
        task_id: u64,
        revision: u64,
    ) -> faktor_core::Result<Option<CompletionContractRow>> {
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::CompletionContractSet {
                task_id: row_task,
                revision: row_revision,
                contract,
            } = entry.payload
            {
                if row_task == task_id && row_revision == revision {
                    return Ok(Some(CompletionContractRow {
                        seq: entry.seq,
                        task_id: row_task,
                        revision: row_revision,
                        contract,
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Append one per-step outcome row. The row must name a revision that
    /// already carries a recorded contract (step statuses are evidence of an
    /// accepted run, never free-floating claims), checked and appended under
    /// the session command lock. Legacy callers record no integration
    /// snapshot ([`Self::ledger_completion_step_status_with_snapshot`] does).
    pub fn ledger_completion_step_status(
        &self,
        task_id: u64,
        revision: u64,
        step: CompletionStep,
        status: CompletionStepOutcome,
        detail: &str,
        at_ms: i64,
    ) -> faktor_core::Result<Option<i64>> {
        self.ledger_completion_step_status_with_snapshot(
            task_id, revision, step, status, detail, None, at_ms,
        )
    }

    /// [`Self::ledger_completion_step_status`] with the final integration
    /// snapshot the outcome is recorded against. A non-empty snapshot must
    /// be the 64-char hex BLAKE3 of the integration root; the completion
    /// gate refuses the step once the root snapshot moved away from it.
    #[allow(clippy::too_many_arguments)]
    pub fn ledger_completion_step_status_with_snapshot(
        &self,
        task_id: u64,
        revision: u64,
        step: CompletionStep,
        status: CompletionStepOutcome,
        detail: &str,
        snapshot: Option<&str>,
        at_ms: i64,
    ) -> faktor_core::Result<Option<i64>> {
        validate_completion_step_status(task_id, revision, detail, at_ms, snapshot)?;
        let _guard = self.command_guard();
        if self
            .ledger_completion_contract_at(task_id, revision)?
            .is_none()
        {
            return Err(SessionError::Conflict(format!(
                "completion step status for task {task_id} revision {revision} has no recorded \
                 contract; record the contract before its step outcomes"
            ))
            .into());
        }
        self.append_entry(LedgerPayload::CompletionStepStatus {
            task_id,
            revision,
            step,
            status,
            detail: detail.to_string(),
            snapshot: snapshot.map(str::to_string),
            at_ms,
        })
    }

    /// Every durable step-status row of one `(task_id, revision)`, ascending
    /// by seq.
    pub fn ledger_completion_step_statuses(
        &self,
        task_id: u64,
        revision: u64,
    ) -> faktor_core::Result<Vec<CompletionStepStatusRow>> {
        let mut out = Vec::new();
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::CompletionStepStatus {
                task_id: row_task,
                revision: row_revision,
                step,
                status,
                detail,
                snapshot,
                at_ms,
            } = entry.payload
            {
                if row_task == task_id && row_revision == revision {
                    out.push(CompletionStepStatusRow {
                        seq: entry.seq,
                        task_id: row_task,
                        revision: row_revision,
                        step,
                        status,
                        detail,
                        snapshot,
                        at_ms,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Append one durable integration record (P0 orchestrated-completion
    /// binding). Record-first: an in-flight record (empty
    /// `final_snapshot_hash`) is legal and is superseded by the finalized
    /// record of the same run. The FULL source/file lists are covered by the
    /// record's exact counts and content digests; the stored samples are
    /// bounded and validated before any byte is journaled.
    pub fn ledger_integration_record_set(
        &self,
        record: &IntegrationRecordRow,
    ) -> faktor_core::Result<i64> {
        validate_integration_record(record)?;
        let _guard = self.command_guard();
        self.append_entry(LedgerPayload::IntegrationRecorded {
            record: record.clone(),
        })?
        .ok_or_else(|| {
            SessionError::Internal("ledger integration record append returned no seq".into()).into()
        })
    }

    /// The NEWEST durable integration record of one task, or `None` when the
    /// task never integrated isolated child changes. Entries ascend by seq,
    /// so the last match wins — the exact record the completion binding must
    /// evaluate.
    pub fn ledger_integration_record_for_task(
        &self,
        task_id: u64,
    ) -> faktor_core::Result<Option<IntegrationRecordRow>> {
        let mut latest: Option<IntegrationRecordRow> = None;
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::IntegrationRecorded { record } = entry.payload {
                if record.task_id == task_id {
                    latest = Some(record);
                }
            }
        }
        Ok(latest)
    }

    /// [`Self::ledger_integration_record_for_task`] with the explicit read
    /// classification (corrupt stream = `PresentMalformed`; failed read =
    /// `StoreFailure`; genuinely absent = `Missing`).
    pub fn ledger_integration_record_for_task_read(
        &self,
        task_id: u64,
    ) -> DurableRead<IntegrationRecordRow> {
        let mut latest: Option<IntegrationRecordRow> = None;
        let entries = match self.all_entries_decoded() {
            Ok(entries) => entries,
            Err(e) => return DurableRead::from_session_error(e),
        };
        for entry in entries {
            if let LedgerPayload::IntegrationRecorded { record } = entry.payload {
                if record.task_id == task_id {
                    latest = Some(record);
                }
            }
        }
        match latest {
            Some(row) => DurableRead::PresentValid(row),
            None => DurableRead::Missing,
        }
    }

    /// Every durable integration record of one task, ascending by seq.
    pub fn ledger_integration_records_for_task(
        &self,
        task_id: u64,
    ) -> faktor_core::Result<Vec<IntegrationRecordRow>> {
        let mut out = Vec::new();
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::IntegrationRecorded { record } = entry.payload {
                if record.task_id == task_id {
                    out.push(record);
                }
            }
        }
        Ok(out)
    }

    /// Append the durable IMMUTABLE run base of one orchestrated run
    /// (recorded before the first child spawn). The row is validated before
    /// any byte is journaled.
    pub fn ledger_run_base_set(&self, record: &RunBaseRecord) -> faktor_core::Result<i64> {
        validate_run_base_record(record)?;
        let _guard = self.command_guard();
        self.append_entry(LedgerPayload::RunBaseRecorded {
            record: record.clone(),
        })?
        .ok_or_else(|| {
            SessionError::Internal("ledger run base append returned no seq".into()).into()
        })
    }

    /// The NEWEST durable run base of one run, or `None` when the run never
    /// recorded one (legacy/direct-runtime runs).
    pub fn ledger_run_base_get(&self, run_id: &str) -> faktor_core::Result<Option<RunBaseRecord>> {
        let mut latest: Option<RunBaseRecord> = None;
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::RunBaseRecorded { record } = entry.payload {
                if record.run_id == run_id {
                    latest = Some(record);
                }
            }
        }
        Ok(latest)
    }

    /// Record (or update) the durable verified-git publication artifact of a
    /// completion contract revision. Newest row wins; the validator bounds
    /// the manifest and the OIDs before anything is appended.
    pub fn ledger_verified_git_artifact_set(
        &self,
        artifact: &VerifiedGitArtifact,
    ) -> faktor_core::Result<i64> {
        validate_verified_git_artifact(artifact)?;
        let _guard = self.command_guard();
        self.append_entry(LedgerPayload::VerifiedGitArtifactRecorded {
            artifact: artifact.clone(),
        })?
        .ok_or_else(|| {
            SessionError::Internal("ledger verified_git_artifact append returned no seq".into())
                .into()
        })
    }

    /// The NEWEST durable verified-git artifact of one (task, revision), or
    /// `None` when the contract revision never published one.
    pub fn ledger_verified_git_artifact_get(
        &self,
        task_id: u64,
        revision: u64,
    ) -> faktor_core::Result<Option<VerifiedGitArtifact>> {
        let mut latest: Option<VerifiedGitArtifact> = None;
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::VerifiedGitArtifactRecorded { artifact } = entry.payload {
                if artifact.task_id == task_id && artifact.revision == revision {
                    latest = Some(artifact);
                }
            }
        }
        Ok(latest)
    }

    /// [`Self::ledger_run_base_get`] with the explicit read classification
    /// (corrupt stream = `PresentMalformed`; failed read = `StoreFailure`;
    /// genuinely absent run base = `Missing`, the legacy/direct-run policy).
    pub fn ledger_run_base_read(&self, run_id: &str) -> DurableRead<RunBaseRecord> {
        let mut latest: Option<RunBaseRecord> = None;
        let entries = match self.all_entries_decoded() {
            Ok(entries) => entries,
            Err(e) => return DurableRead::from_session_error(e),
        };
        for entry in entries {
            if let LedgerPayload::RunBaseRecorded { record } = entry.payload {
                if record.run_id == run_id {
                    latest = Some(record);
                }
            }
        }
        match latest {
            Some(row) => DurableRead::PresentValid(row),
            None => DurableRead::Missing,
        }
    }

    /// Append one durable external-operation row. The row is validated
    /// BEFORE it is journaled (bounded shape, state/remote-identity
    /// coherence and the deterministic content id), so a hostile or
    /// hand-mismatched id can never enter the stream.
    pub fn ledger_external_operation_set(
        &self,
        row: &ExternalOperationRow,
    ) -> faktor_core::Result<i64> {
        validate_external_operation(row)?;
        self.append_typed_entry(LedgerPayload::ExternalOperationRecorded {
            record: row.clone(),
        })
    }

    /// The LATEST durable row of one operation key. The explicit read
    /// distinction is preserved: a corrupt stream is `PresentMalformed` and
    /// a failed read is `StoreFailure` — never "no operation recorded".
    pub fn ledger_external_operation_read(
        &self,
        operation_key: &str,
    ) -> DurableRead<ExternalOperationRow> {
        let mut latest: Option<ExternalOperationRow> = None;
        let entries = match self.all_entries_decoded() {
            Ok(entries) => entries,
            Err(e) => return DurableRead::from_session_error(e),
        };
        for entry in entries {
            if let LedgerPayload::ExternalOperationRecorded { record } = entry.payload {
                if record.operation_key == operation_key {
                    latest = Some(record);
                }
            }
        }
        match latest {
            Some(row) => DurableRead::PresentValid(row),
            None => DurableRead::Missing,
        }
    }

    /// Every durable external-operation row, ascending by seq (the
    /// reconciliation scan surface). A corrupt row refuses the whole read.
    pub fn ledger_external_operations(&self) -> faktor_core::Result<Vec<ExternalOperationRow>> {
        let mut out = Vec::new();
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::ExternalOperationRecorded { record } = entry.payload {
                out.push(record);
            }
        }
        Ok(out)
    }

    /// Append one durable landing-transaction row (record-first: decisions +
    /// rollback blobs before the first owner write; later rows journal the
    /// per-path outcomes and the phase).
    pub fn ledger_integration_txn_set(&self, row: &IntegrationTxnRow) -> faktor_core::Result<i64> {
        validate_integration_txn(row)?;
        let _guard = self.command_guard();
        self.append_entry(LedgerPayload::IntegrationTxnRecorded { row: row.clone() })?
            .ok_or_else(|| {
                SessionError::Internal("ledger integration txn append returned no seq".into())
                    .into()
            })
    }

    /// The NEWEST durable landing transaction of one run, or `None`.
    pub fn ledger_integration_txn_for_run(
        &self,
        run_id: &str,
    ) -> faktor_core::Result<Option<IntegrationTxnRow>> {
        let mut latest: Option<IntegrationTxnRow> = None;
        for entry in self.all_entries_decoded()? {
            if let LedgerPayload::IntegrationTxnRecorded { row } = entry.payload {
                if row.run_id == run_id {
                    latest = Some(row);
                }
            }
        }
        Ok(latest)
    }

    /// [`Self::ledger_integration_txn_for_run`] with the explicit read
    /// classification (corrupt stream = `PresentMalformed`; failed read =
    /// `StoreFailure`; genuinely absent transaction = `Missing`).
    pub fn ledger_integration_txn_read(&self, run_id: &str) -> DurableRead<IntegrationTxnRow> {
        let mut latest: Option<IntegrationTxnRow> = None;
        let entries = match self.all_entries_decoded() {
            Ok(entries) => entries,
            Err(e) => return DurableRead::from_session_error(e),
        };
        for entry in entries {
            if let LedgerPayload::IntegrationTxnRecorded { row } = entry.payload {
                if row.run_id == run_id {
                    latest = Some(row);
                }
            }
        }
        match latest {
            Some(row) => DurableRead::PresentValid(row),
            None => DurableRead::Missing,
        }
    }

    /// Append one `TerminalCreated` row (record-first: the durable ownership
    /// row exists before the row is exposed by any adapter).
    pub fn ledger_terminal_created(&self, row: &TerminalDurableRow) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_created")?;
        self.append_typed_entry(LedgerPayload::TerminalCreated { row: row.clone() })
    }

    /// Append one `TerminalRunning` row (the child is up and its process
    /// identity is known).
    pub fn ledger_terminal_running(&self, row: &TerminalDurableRow) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_running")?;
        self.append_typed_entry(LedgerPayload::TerminalRunning { row: row.clone() })
    }

    /// Append one `TerminalExited` row (the process was observed dead).
    pub fn ledger_terminal_exited(
        &self,
        row: &TerminalDurableRow,
        exit_code: Option<i32>,
    ) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_exited")?;
        self.append_typed_entry(LedgerPayload::TerminalExited {
            row: row.clone(),
            exit_code,
        })
    }

    /// Append one `TerminalKilled` row (a kill routed through the pty
    /// authority terminated the tree).
    pub fn ledger_terminal_killed(
        &self,
        row: &TerminalDurableRow,
        reason: &str,
    ) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_killed")?;
        validate_terminal_detail(reason, "terminal_killed reason")?;
        self.append_typed_entry(LedgerPayload::TerminalKilled {
            row: row.clone(),
            reason: reason.to_string(),
        })
    }

    /// Append one `TerminalLost` row (a restart found no live authority for
    /// the row; the process identity decides the audit reason).
    pub fn ledger_terminal_lost(
        &self,
        row: &TerminalDurableRow,
        reason: &str,
    ) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_lost")?;
        validate_terminal_detail(reason, "terminal_lost reason")?;
        self.append_typed_entry(LedgerPayload::TerminalLost {
            row: row.clone(),
            reason: reason.to_string(),
        })
    }

    /// Append one `TerminalReconciled` row (a stale Lost row finished typed;
    /// `disposition` is `killed` or `collected`).
    pub fn ledger_terminal_reconciled(
        &self,
        row: &TerminalDurableRow,
        disposition: &str,
    ) -> faktor_core::Result<i64> {
        validate_terminal_row(row, "terminal_reconciled")?;
        if !matches!(
            disposition,
            TERMINAL_RECONCILE_KILLED | TERMINAL_RECONCILE_COLLECTED
        ) {
            return Err(SessionError::Malformed(format!(
                "terminal reconcile disposition {disposition:?} is not killed|collected"
            ))
            .into());
        }
        self.append_typed_entry(LedgerPayload::TerminalReconciled {
            row: row.clone(),
            disposition: disposition.to_string(),
        })
    }

    /// Every durable terminal row of this session above `after_seq`
    /// (ascending), strictly decoded. The stream is the authority the
    /// terminal service folds into per-terminal state; a corrupt terminal
    /// row is a loud error, never a silent drop.
    pub fn ledger_terminal_rows(
        &self,
        after_seq: Option<i64>,
    ) -> faktor_core::Result<Vec<TerminalLedgerRecord>> {
        let store = self.manager.store();
        let mut cursor = after_seq;
        let mut out: Vec<TerminalLedgerRecord> = Vec::new();
        loop {
            let page = store
                .ledger_entries(self.id, cursor, MAX_LEDGER_PAGE)
                .map_err(map_store_err)?;
            if page.is_empty() {
                break;
            }
            cursor = page.last().map(|r| r.seq);
            for row in page {
                let entry = self.decode_row(&row)?;
                if let Some(record) = terminal_record_of(entry) {
                    out.push(record);
                }
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// The shared typed append tail: bounds the payload, maps its entry
    /// type, and writes the single row (gapless, always above the head's
    /// checkpoint so the fold cursor never rewinds).
    fn append_entry(&self, payload: LedgerPayload) -> faktor_core::Result<Option<i64>> {
        check_payload_bytes(&payload)?;
        let tag = entry_tag_of(&payload);
        let json = serde_json::to_value(&payload)
            .map_err(|e| SessionError::Internal(format!("ledger payload serialization: {e}")))?;
        let seq = self
            .manager
            .store()
            .append_ledger_entry(self.id, tag, LEDGER_ENTRY_SCHEMA_V, json)
            .map_err(map_store_err)?;
        Ok(Some(seq))
    }

    /// Crate-internal typed append seam for additive durable kinds owned by
    /// sibling modules (the coordination board): the SAME payload-bound and
    /// tag tail as every typed accessor, so a sibling can never bypass the
    /// ledger entry bound. The caller validates the payload's shape via the
    /// shared `pub(crate)` validators before calling.
    pub(crate) fn append_typed_entry(&self, payload: LedgerPayload) -> faktor_core::Result<i64> {
        self.append_entry(payload)?.ok_or_else(|| {
            SessionError::Internal("ledger typed append returned no seq".into()).into()
        })
    }

    // ------------------------------------------------------------ compaction

    /// Watermark compaction of the typed ledger. Reads and strictly decodes
    /// the ENTIRE entry stream first (an undecodable row refuses the
    /// compaction loudly — nothing is ever silently deleted), then deletes
    /// every entry below the watermark EXCEPT the pinned never-evict set:
    /// the last GoalSet, the last CriteriaSet, the last Decision and every
    /// unresolved BlockerOpened. The head checkpoint is rewritten in the
    /// SAME transaction as the deletion, folded from the pre-prune stream
    /// (compaction is projection-preserving, never FIFO-evicting).
    pub fn compact_typed_ledger(&self) -> faktor_core::Result<LedgerCompactReport> {
        let entries = self.all_entries_decoded()?;
        let entries_before = entries.len() as i64;
        if entries.is_empty() {
            return Ok(LedgerCompactReport {
                entries_before: 0,
                deleted: 0,
                kept: 0,
                pinned: Vec::new(),
                checkpoint_seq: 0,
            });
        }
        // Current materialized head FIRST: it already folds everything the
        // stream holds plus what EARLIER compactions pruned (epochs, child
        // records, routing tails...). Compaction folds onto it — a
        // full-stream replay would silently forget head-only content.
        let mut head = self.ledger_ensure_head()?;
        let checkpoint_seq = entries.last().expect("non-empty").seq;
        if head.checkpoint_seq < checkpoint_seq {
            let after = Some(head.checkpoint_seq);
            let rows = self
                .manager
                .store()
                .ledger_entries(self.id, after, u64::MAX)
                .map_err(map_store_err)?;
            let mut extra = Vec::with_capacity(rows.len());
            for row in rows {
                extra.push(self.decode_row(&row)?);
            }
            fold_entries(&mut head, &extra)?;
        }
        // Pinned never-evict set: the LAST GoalSet / CriteriaSet / Decision
        // entry, EVERY unresolved BlockerOpened's own opener entry, and
        // every row of an OPEN edit transaction (its `edit_txn_prepared`
        // plus each `edit_txn_progress` — together they are the durable
        // recovery record of an unfinished multi-file edit). Closed
        // transactions (a terminal row exists) are NOT foldable into the
        // head and age out below the watermark like any finished row.
        let mut pinned: Vec<i64> = Vec::new();
        let mut last: BTreeMap<&'static str, i64> = BTreeMap::new();
        let mut open_opener_seqs: Vec<i64> = Vec::new();
        // Learning corpus rows (audits 65-67/92) are pinned: they are the
        // durable corpus `faktor-learning` reopens from, not turn history —
        // watermark compaction must never silently delete a mined learning.
        let mut learning_seqs: Vec<i64> = Vec::new();
        // Tournament rows are pinned the same way: the whole lifecycle
        // (`TournamentStarted`/`CandidateSettled`/`TournamentDecided`) is
        // the durable authority a crashed executor re-opens from, so
        // compaction must never silently delete a tournament history.
        let mut tournament_seqs: Vec<i64> = Vec::new();
        // Board rows are pinned too: the append-only stream IS the live
        // board surface (posts, reads, receipts, reset markers) — a
        // compacted ledger must still reconstruct every visible post and
        // every unread count exactly.
        let mut board_seqs: Vec<i64> = Vec::new();
        // Completion-contract rows are pinned: the accepted contract and
        // every per-step outcome are the durable authority the completion
        // gate reads, so watermark compaction must never delete them (a
        // pruned status row would silently turn "not done" into "done").
        let mut completion_seqs: Vec<i64> = Vec::new();
        // Integration records are pinned: the root verification binding and
        // the completion gate re-read them, so a pruned record would
        // silently unbind a passing verification from the root it certified.
        let mut integration_seqs: Vec<i64> = Vec::new();
        // Run-base records and landing-transaction rows are pinned: staging
        // refuses against the recorded base and crash recovery finishes or
        // rolls back from the transaction rows, so watermark compaction must
        // never silently delete either authority.
        let mut orchestrated_txn_seqs: Vec<i64> = Vec::new();
        // Terminal-lifecycle rows are pinned: they are the durable authority
        // the terminal service reconstructs every row's state from after a
        // restart, so watermark compaction must never silently delete a
        // terminal's history (a pruned row would turn a Lost terminal into an
        // unknowable one).
        let mut terminal_seqs: Vec<i64> = Vec::new();
        // External-operation rows are pinned too: a restart reconciles an
        // unfinished external effect from the EXACT recorded input identity,
        // so a compacted ledger must still hold every prepared/completed row.
        let mut external_operation_seqs: Vec<i64> = Vec::new();
        for entry in &entries {
            match &entry.payload {
                LedgerPayload::GoalSet { .. } => {
                    last.insert(ENTRY_GOAL_SET, entry.seq);
                }
                LedgerPayload::CriteriaSet { .. } => {
                    last.insert(ENTRY_CRITERIA_SET, entry.seq);
                }
                LedgerPayload::Decision { .. } => {
                    last.insert(ENTRY_DECISION, entry.seq);
                }
                LedgerPayload::BlockerOpened { reason } => open_opener_seqs.extend(
                    head.open_blockers
                        .iter()
                        .any(|r| r == reason)
                        .then_some(entry.seq),
                ),
                LedgerPayload::LearningRecord { .. } => learning_seqs.push(entry.seq),
                LedgerPayload::TournamentStarted { .. }
                | LedgerPayload::CandidateSettled { .. }
                | LedgerPayload::TournamentDecided { .. } => tournament_seqs.push(entry.seq),
                LedgerPayload::BoardPost { .. }
                | LedgerPayload::BoardRead { .. }
                | LedgerPayload::BoardReceipt { .. }
                | LedgerPayload::BoardReset { .. } => board_seqs.push(entry.seq),
                LedgerPayload::CompletionContractSet { .. }
                | LedgerPayload::CompletionStepStatus { .. } => completion_seqs.push(entry.seq),
                LedgerPayload::IntegrationRecorded { .. } => integration_seqs.push(entry.seq),
                LedgerPayload::RunBaseRecorded { .. }
                | LedgerPayload::IntegrationTxnRecorded { .. } => {
                    orchestrated_txn_seqs.push(entry.seq)
                }
                LedgerPayload::ExternalOperationRecorded { .. } => {
                    external_operation_seqs.push(entry.seq)
                }
                LedgerPayload::TerminalCreated { .. }
                | LedgerPayload::TerminalRunning { .. }
                | LedgerPayload::TerminalExited { .. }
                | LedgerPayload::TerminalKilled { .. }
                | LedgerPayload::TerminalLost { .. }
                | LedgerPayload::TerminalReconciled { .. } => terminal_seqs.push(entry.seq),
                _ => {}
            }
        }
        // Open edit txn = prepared rows whose txn_id has no terminal row.
        let mut prepared_ids: BTreeMap<u64, ()> = BTreeMap::new();
        let mut terminal_ids: BTreeMap<u64, ()> = BTreeMap::new();
        for entry in &entries {
            match &entry.payload {
                LedgerPayload::EditTxnPrepared { txn_id, .. } => {
                    prepared_ids.insert(*txn_id, ());
                }
                LedgerPayload::EditTxnCommitted { txn_id, .. }
                | LedgerPayload::EditTxnRolledBack { txn_id, .. } => {
                    terminal_ids.insert(*txn_id, ());
                }
                _ => {}
            }
        }
        for entry in &entries {
            let txn = match &entry.payload {
                LedgerPayload::EditTxnPrepared { txn_id, .. }
                | LedgerPayload::EditTxnProgress { txn_id, .. } => Some(*txn_id),
                _ => None,
            };
            if txn
                .is_some_and(|id| prepared_ids.contains_key(&id) && !terminal_ids.contains_key(&id))
            {
                pinned.push(entry.seq);
            }
        }
        for seq in last.values() {
            pinned.push(*seq);
        }
        pinned.extend(open_opener_seqs);
        pinned.extend(learning_seqs);
        pinned.extend(tournament_seqs);
        pinned.extend(board_seqs);
        pinned.extend(completion_seqs);
        pinned.extend(integration_seqs);
        pinned.extend(orchestrated_txn_seqs);
        pinned.extend(terminal_seqs);
        pinned.extend(external_operation_seqs);
        pinned.sort_unstable();
        pinned.dedup();
        let head_json = head_to_json(&head)?;
        let head_value = serde_json::from_str(&head_json)
            .map_err(|e| SessionError::Internal(format!("head json: {e}")))?;
        let deleted = self
            .manager
            .store()
            .compact_ledger(
                self.id,
                checkpoint_seq + 1,
                &pinned,
                head_value,
                checkpoint_seq,
                LEDGER_HEAD_SCHEMA_V,
            )
            .map_err(map_store_err)? as i64;
        Ok(LedgerCompactReport {
            entries_before,
            deleted,
            kept: entries_before - deleted,
            pinned,
            checkpoint_seq,
        })
    }

    /// The materialized typed view: the head fold + entry count. This is
    /// the read surface the runtime and the never-lose tests assert on.
    pub fn ledger_view(&self) -> faktor_core::Result<crate::ledger::LedgerView> {
        let head = self.ledger_ensure_head()?;
        let entry_count = self
            .manager
            .store()
            .ledger_max_seq(self.id)
            .map_err(map_store_err)?;
        Ok(LedgerView { head, entry_count })
    }

    /// Session-open ledger verification (audit 27 never-lose contract): the
    /// typed entry stream is strictly decoded in FULL and the head is
    /// brought current. An entry that fails its schema decode (unknown
    /// version, unknown type, shape violation) FAILS THE OPEN loudly — the
    /// ledger never silently drops a row. Called on every handle open
    /// (manager get/list); no entries => a fast no-op.
    pub fn ledger_verify_open(&self) -> faktor_core::Result<()> {
        if self
            .manager
            .store()
            .ledger_max_seq(self.id)
            .map_err(map_store_err)?
            == 0
        {
            return Ok(());
        }
        let _ = self.all_entries_decoded()?;
        let _ = self.ledger_ensure_head()?;
        Ok(())
    }
}

/// The session's typed ledger view: the materialized head + entry count.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerView {
    pub head: LedgerHead,
    /// Newest entry seq (0 = empty ledger). After a compaction this can sit
    /// below `head.checkpoint_seq` (the head is folded AHEAD of the pruned
    /// stream by design).
    pub entry_count: i64,
}

/// True when `reason` names a currently open blocker (view read helper used
/// by the runtime gate integration).
pub fn blocker_is_open(head: &LedgerHead, reason: &str) -> bool {
    head.open_blockers.iter().any(|r| r == reason)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use std::sync::Arc;

    fn raw_sql(m: &crate::SessionManager, sql: &str) {
        m.store().sql_execute(sql).unwrap();
    }

    fn turn_entries(s: &SessionHandle, turn: u64) {
        s.ledger_decision(
            &format!("step-{turn}"),
            &format!("choice-{turn}"),
            &format!("rationale-{turn}"),
        )
        .unwrap();
        s.ledger_routing_decision(
            turn,
            &format!("prov-{turn}"),
            &format!("model-{turn}"),
            "quality fit",
            42,
        )
        .unwrap();
        s.ledger_plan_step_added(
            turn as u32,
            &format!("do step {turn}"),
            Some(turn as u32 - 1),
        )
        .unwrap();
        s.ledger_verify_run(
            &[LedgerCheckRun {
                id: format!("check-{turn}"),
                passed: true,
            }],
            "passed",
        )
        .unwrap();
        s.ledger_turn_completed(turn).unwrap();
    }

    #[test]
    fn verified_git_artifact_roundtrips_and_refuses_hostile_rows() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let state = faktor_fs::entry_state::EntryState::regular(
            faktor_fs::tree_manifest::CanonicalMode::ExecutableFile,
            faktor_core::hash::FileHash::from(blake3::hash(b"payload").into()),
        )
        .unwrap();
        let artifact = VerifiedGitArtifact {
            task_id: 7,
            revision: 3,
            verification_record: 11,
            verified_root_digest: "tm1:abc".into(),
            verified_manifest: vec![
                VerifiedManifestEntry {
                    path: "a/b.rs".into(),
                    state: state.clone(),
                },
                VerifiedManifestEntry {
                    path: "link".into(),
                    state: faktor_fs::entry_state::EntryState::symlink(b"a/b.rs".to_vec()).unwrap(),
                },
            ],
            git_tree_oid: Some("a".repeat(40)),
            commit_oid: None,
            local_ref: None,
            remote_ref: None,
            updated_ms: 1,
        };
        s.ledger_verified_git_artifact_set(&artifact).unwrap();
        let read = s
            .ledger_verified_git_artifact_get(7, 3)
            .unwrap()
            .expect("roundtrip");
        assert_eq!(read, artifact);
        assert!(s.ledger_verified_git_artifact_get(7, 4).unwrap().is_none());
        // A hostile manifest (traversal path) is refused BEFORE any append.
        let mut evil = artifact.clone();
        evil.verified_manifest[0].path = "../escape.rs".into();
        assert!(s.ledger_verified_git_artifact_set(&evil).is_err());
        // An unsorted manifest is refused too (canonical order is the codec).
        let mut unsorted = artifact.clone();
        unsorted.verified_manifest.swap(0, 1);
        assert!(s.ledger_verified_git_artifact_set(&unsorted).is_err());
        // A hostile oid is refused.
        let mut bad_oid = artifact.clone();
        bad_oid.commit_oid = Some("zz".repeat(20));
        assert!(s.ledger_verified_git_artifact_set(&bad_oid).is_err());
        // A malformed encoded remote ref is refused.
        let mut bad_remote = artifact.clone();
        bad_remote.remote_ref = Some("origin:refs/heads/main@nope".into());
        assert!(s.ledger_verified_git_artifact_set(&bad_remote).is_err());
        let good_remote = VerifiedGitArtifact {
            remote_ref: Some(format!("origin:refs/heads/main@{}", "b".repeat(40))),
            commit_oid: Some("b".repeat(40)),
            local_ref: Some("refs/heads/main".into()),
            ..artifact
        };
        s.ledger_verified_git_artifact_set(&good_remote).unwrap();
        let read = s.ledger_verified_git_artifact_get(7, 3).unwrap().unwrap();
        assert_eq!(read.remote_ref, good_remote.remote_ref);
    }

    fn assert_never_lost(s: &SessionHandle, goal: &str, open_blockers: &[&str]) {
        let view = s.ledger_view().unwrap();
        assert_eq!(view.head.goal, goal, "goal survives compaction");
        assert!(
            !view.head.criteria.is_empty(),
            "criteria survive compaction"
        );
        for b in open_blockers {
            assert!(
                view.head.open_blockers.iter().any(|o| o == b),
                "open blocker {b} must survive compaction"
            );
        }
        assert!(
            !view.head.decisions.is_empty(),
            "last decision must survive compaction"
        );
        // Entry-level: the pinned rows still exist in the stream.
        let mut page = s.ledger_entries_page(None, 500).unwrap();
        let mut entries = Vec::new();
        loop {
            let cursor = page.entries.last().map(|e| e.seq);
            entries.extend(page.entries);
            if !page.has_more {
                break;
            }
            page = s
                .ledger_entries_page(cursor, 500)
                .expect("paged entries decode");
        }
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.payload, LedgerPayload::GoalSet { goal: g } if g == goal)),
            "GoalSet entry must survive"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.payload, LedgerPayload::CriteriaSet { .. })),
            "CriteriaSet entry must survive"
        );
        assert!(
            entries
                .iter()
                .any(|e| matches!(&e.payload, LedgerPayload::Decision { .. })),
            "last Decision entry must survive"
        );
        for b in open_blockers {
            assert!(
                entries.iter().any(|e| matches!(
                    &e.payload,
                    LedgerPayload::BlockerOpened { reason } if reason == b
                )),
                "unresolved BlockerOpened {b} entry must survive"
            );
        }
        let head = s.manager.store().ledger_head(s.id).unwrap();
        assert!(
            head.is_some() && head.unwrap().checkpoint_seq > 0,
            "latest checkpoint head must be present"
        );
    }

    #[test]
    fn typed_child_blocker_delegates_to_the_existing_string_payload() {
        use faktor_core::blocker::{BlockerKind, ChildBlocker, MAX_CHILD_BLOCKER_REASON_CHARS};

        let (_d, m) = test_manager();
        let s = session(&m);
        let blocker = ChildBlocker::new(
            BlockerKind::External,
            "remote deploy is pending",
            "wait for the upstream deploy, then resume",
        );
        // Open through the TYPED delegation: the durable row is still the
        // historic string `BlockerOpened` payload (additive compat).
        let seq = s
            .ledger_child_blocker_opened(&blocker)
            .unwrap()
            .expect("typed open writes the string row");
        assert!(seq > 0);
        let view = s.ledger_view().unwrap();
        assert!(view
            .head
            .open_blockers
            .iter()
            .any(|r| r == &blocker.ledger_reason()));
        let entries = collect_all(&s);
        assert!(entries.iter().any(|e| matches!(
            &e.payload,
            LedgerPayload::BlockerOpened { reason } if reason == &blocker.reason
        )));
        // Re-opening the same typed reason is the idempotent no-op.
        assert!(s.ledger_child_blocker_opened(&blocker).unwrap().is_none());
        // A hostile typed blocker is refused by the shared validator BEFORE
        // any ledger write.
        let huge = ChildBlocker::new(
            BlockerKind::Unknown,
            "x".repeat(MAX_CHILD_BLOCKER_REASON_CHARS + 1),
            "resolve",
        );
        assert!(s.ledger_child_blocker_opened(&huge).is_err());
        // Resolve through the same typed delegation.
        s.ledger_child_blocker_resolved(&blocker).unwrap();
        assert!(!s
            .ledger_view()
            .unwrap()
            .head
            .open_blockers
            .iter()
            .any(|r| r == &blocker.ledger_reason()));
        assert!(entries.iter().any(|e| matches!(
            &e.payload,
            LedgerPayload::BlockerOpened { reason } if reason == &blocker.reason
        )));
    }

    #[test]
    fn typed_accessors_roundtrip_and_view_folds() {
        let (_d, m) = test_manager();
        let s = session(&m);
        assert!(s.ledger_view().unwrap().head.goal.is_empty());
        s.ledger_goal_set("implement the ledger").unwrap();
        s.ledger_criteria_set(
            &["cargo test".into(), "no warnings".into()],
            "cargo test + no warnings",
        )
        .unwrap();
        s.ledger_blocker_opened("check alpha failed").unwrap();
        s.ledger_decision("pick", "rust", "ecosystem").unwrap();
        s.ledger_plan_step_added(0, "read the spec", None).unwrap();
        s.ledger_plan_step_added(1, "write code", Some(0)).unwrap();
        s.ledger_routing_decision(9, "ollama", "qwen3.8", "cost", 3)
            .unwrap();
        s.ledger_epoch_bumped(None, 7).unwrap();
        s.ledger_failure_recorded("test failed: x").unwrap();
        s.ledger_turn_completed(9).unwrap();
        let view = s.ledger_view().unwrap();
        assert_eq!(view.head.goal, "implement the ledger");
        assert_eq!(view.head.criteria, vec!["cargo test", "no warnings"]);
        assert_eq!(view.head.open_blockers, vec!["check alpha failed"]);
        assert_eq!(view.head.epoch, Some(7));
        assert_eq!(view.head.routing_count, 1);
        assert_eq!(view.head.routing_tail[0].provider, "ollama");
        assert_eq!(view.head.plan_steps[1].parent_index, Some(0));
        // Blocker resolution removes the reason from the fold.
        s.ledger_blocker_resolved("check alpha failed").unwrap();
        let view = s.ledger_view().unwrap();
        assert!(view.head.open_blockers.is_empty());
        // Duplicate open of the same reason is a no-op, never an error.
        s.ledger_blocker_opened("again").unwrap();
        assert!(s.ledger_blocker_opened("again").unwrap().is_none());
        assert_eq!(
            s.ledger_view().unwrap().head.open_blockers,
            vec!["again".to_string()]
        );
        // Resolving a reason that is not open is a typed error.
        let err = s.ledger_blocker_resolved("never-opened").unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict);
    }

    #[test]
    fn out_of_order_child_finish_is_a_typed_append_error() {
        let (_d, m) = test_manager();
        let s = session(&m);
        // Finish without start: typed error at append time.
        let err = s.ledger_child_finished(77, "done").unwrap_err();
        assert!(err.to_string().contains("no open start"), "{err}");
        // Start -> finish is legal.
        s.ledger_child_started(77, 3, 2, "verify the change")
            .unwrap();
        s.ledger_child_finished(77, "verified").unwrap();
        // Double finish is again a typed error.
        let err = s.ledger_child_finished(77, "again").unwrap_err();
        assert!(err.to_string().contains("no open start"), "{err}");
        // Double START of the same running agent conflicts.
        s.ledger_child_started(78, 3, 2, "second").unwrap();
        let err = s.ledger_child_started(78, 3, 2, "dup").unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict);
        // A finished agent may start again.
        s.ledger_child_finished(78, "done").unwrap();
        s.ledger_child_started(78, 3, 2, "restart").unwrap();
        assert!(s.ledger_child_finished(78, "done again").is_ok());
        // Zero ids are malformed.
        assert!(s.ledger_child_started(0, 1, 1, "x").is_err());
        // The fold keeps children with their outcomes.
        let view = s.ledger_view().unwrap();
        assert_eq!(view.head.children.len(), 2);
        assert_eq!(
            view.head
                .children
                .iter()
                .find(|c| c.agent_id == 77)
                .unwrap()
                .outcome
                .as_deref(),
            Some("verified")
        );
    }

    #[test]
    fn child_presentation_rows_are_strict_and_fold_the_latest() {
        let (_d, m) = test_manager();
        let s = session(&m);
        // Hostile raw rows are loud on read, never silently parsed.
        let hostile: Vec<serde_json::Value> = vec![
            serde_json::json!({"kind":"child_presentation_changed","child_id":"","from":"foreground","to":"background","at_ms":1}),
            serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"foreground","at_ms":1}),
            serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"background","at_ms":0}),
            serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","to":"paused","at_ms":1}),
            serde_json::json!({"kind":"child_presentation_changed","child_id":"a/b","from":"foreground","to":"background","at_ms":1}),
            serde_json::json!({"kind":"child_presentation_changed","child_id":"child-0","from":"foreground","at_ms":1}),
        ];
        for row in &hostile {
            assert!(
                decode_payload(ENTRY_CHILD_PRESENTATION_CHANGED, LEDGER_ENTRY_SCHEMA_V, row)
                    .is_err(),
                "hostile presentation row must be refused: {row}"
            );
        }
        // Legal rows fold the LATEST state per child into the head.
        for (child, from, to, at) in [
            (
                "child-0",
                PresentationState::Foreground,
                PresentationState::Background,
                1,
            ),
            (
                "child-0",
                PresentationState::Background,
                PresentationState::Foreground,
                2,
            ),
            (
                "child-0",
                PresentationState::Foreground,
                PresentationState::Background,
                3,
            ),
            (
                "child-1",
                PresentationState::Foreground,
                PresentationState::Background,
                4,
            ),
        ] {
            s.append_typed_entry(LedgerPayload::ChildPresentationChanged {
                child_id: child.into(),
                from,
                to,
                at_ms: at,
            })
            .unwrap();
        }
        assert_eq!(
            s.child_presentation("child-0").unwrap(),
            PresentationState::Background
        );
        assert_eq!(
            s.child_presentation("child-1").unwrap(),
            PresentationState::Background
        );
        assert_eq!(
            s.child_presentation("child-2").unwrap(),
            PresentationState::Foreground
        );
        // Compaction may prune the rows; the folded head keeps the latest.
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.deleted > 0);
        assert_eq!(
            s.child_presentation("child-0").unwrap(),
            PresentationState::Background
        );
        assert!(s
            .ledger_view()
            .unwrap()
            .head
            .presentations
            .contains_key("child-1"));
    }

    #[test]
    fn never_lose_survives_20_turns_and_6_compactions() {
        // Requirement 4a: 20 turns with decisions/blocks/children, 6
        // compactions, then GoalSet + CriteriaSet + the last Decision +
        // every unresolved BlockerOpened and the latest head are all there.
        let (_d, m) = test_manager();
        let s = session(&m);
        s.ledger_goal_set("goal-0").unwrap();
        s.ledger_criteria_set(&["c0".into(), "c1".into()], "c0 c1")
            .unwrap();
        s.ledger_epoch_bumped(None, 1).unwrap();
        for turn in 1..=20u64 {
            turn_entries(&s, turn);
            if turn == 5 {
                s.ledger_child_started(500 + turn, turn, 1, "subagent verify")
                    .unwrap();
                s.ledger_child_finished(500 + turn, "done").unwrap();
            }
            if turn == 7 {
                s.ledger_blocker_opened("blocker-seven").unwrap();
            }
            if turn == 13 {
                s.ledger_blocker_opened("blocker-thirteen").unwrap();
            }
            if turn == 9 {
                // A blocker opened and later resolved must NOT linger.
                s.ledger_blocker_opened("resolved-nine").unwrap();
                s.ledger_blocker_resolved("resolved-nine").unwrap();
            }
            if turn % 3 == 0 {
                s.compact_typed_ledger().unwrap();
            }
        }
        // Three more compactions beyond the turns.
        for _ in 0..3 {
            s.compact_typed_ledger().unwrap();
        }
        assert_never_lost(&s, "goal-0", &["blocker-seven", "blocker-thirteen"]);
        let view = s.ledger_view().unwrap();
        // The last Decision entry is the decision of turn 20.
        let last = view.head.decisions.first().unwrap();
        assert_eq!(last.step, "step-20");
        assert_eq!(view.head.epoch, Some(1));
        assert_eq!(view.head.routing_count, 20);
        // Head-only content that compaction pruned from the entry stream is
        // still folded in the head: the child-agent records of turn 5 and
        // the plan DAG of the compacted steps.
        assert_eq!(view.head.children.len(), 1, "children survive in the head");
        assert_eq!(view.head.children[0].agent_id, 505);
        assert!(
            !view.head.plan_steps.is_empty(),
            "plan DAG survives in the head"
        );
        // Compaction pruned the stream: entries are bounded to the pinned
        // set + nothing newer (goal, criteria, 2 blockers, 1 decision).
        let entries = collect_all(&s);
        assert_eq!(entries.len(), 5, "only pinned entries survive: {entries:?}");
        // Resolved blockers do not linger in the fold.
        assert!(!view.head.open_blockers.iter().any(|r| r == "resolved-nine"));
    }

    fn collect_all(s: &SessionHandle) -> Vec<TypedLedgerEntry> {
        let mut out = Vec::new();
        let mut cursor = None;
        loop {
            let page = s.ledger_entries_page(cursor, 500).unwrap();
            let c = page.entries.last().map(|e| e.seq);
            out.extend(page.entries);
            if !page.has_more {
                break;
            }
            cursor = c;
        }
        out
    }

    #[test]
    fn watermark_holds_across_five_compacting_turns() {
        // Requirement 3's watermark test: five turns, each compacting.
        let (_d, m) = test_manager();
        let s = session(&m);
        s.ledger_goal_set("the-goal").unwrap();
        s.ledger_criteria_set(&["must compile".into()], "must compile")
            .unwrap();
        for turn in 1..=5u64 {
            s.ledger_blocker_opened(&format!("blocker-{turn}")).unwrap();
            turn_entries(&s, turn);
            s.compact_typed_ledger().unwrap();
            // After EVERY compaction all protected content is present.
            assert_never_lost(&s, "the-goal", &[&format!("blocker-{turn}")]);
        }
        // With five open blockers, all five survive compaction.
        s.compact_typed_ledger().unwrap();
        assert_never_lost(
            &s,
            "the-goal",
            &[
                "blocker-1",
                "blocker-2",
                "blocker-3",
                "blocker-4",
                "blocker-5",
            ],
        );
    }

    #[test]
    fn crafted_unknown_schema_version_makes_every_typed_reader_error_loudly() {
        // Requirement 4b: a row written with schema_ver 999 makes every
        // typed reader error loudly (Corrupt), and compaction refuses.
        let (_d, m) = test_manager();
        let s = session(&m);
        s.ledger_goal_set("g").unwrap();
        raw_sql(
            &m,
            &format!(
                "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES ({}, 2, 'goal_set', 999, '{{\"goal\": \"future\"}}', 1)",
                s.id().raw()
            ),
        );
        // Every typed reader fails loudly.
        let err = s.ledger_view().unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
        let err = s.ledger_verify_open().unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
        let err = s.ledger_entries_page(None, 10).unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
        // Compaction refuses to checkpoint it: nothing deleted, head intact.
        let head_before = s.manager.store().ledger_head(s.id()).unwrap();
        let err = s.compact_typed_ledger().unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
        let head_after = s.manager.store().ledger_head(s.id()).unwrap();
        assert_eq!(head_before, head_after, "compaction must not checkpoint");
        let rows = s.manager.store().ledger_entries(s.id(), None, 10).unwrap();
        assert_eq!(rows.len(), 2, "nothing was deleted around the corrupt row");
        // Reopening the session fails loudly too.
        let err = m.get_session(s.id()).unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
    }

    #[test]
    fn payload_shape_violation_is_loud_never_silent() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.ledger_goal_set("g").unwrap();
        // Valid JSON, wrong shape for its tag: strict decode fails loudly.
        raw_sql(
            &m,
            &format!(
                "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
                 VALUES ({}, 2, 'goal_set', 1, '{{\"nonsense\": true}}', 1)",
                s.id().raw()
            ),
        );
        let err = s.ledger_view().unwrap_err();
        assert!(err.to_string().contains("v1 schema"), "{err}");
        assert!(m.get_session(s.id()).is_err());
    }

    #[test]
    fn concurrent_appends_interleave_without_losing_entries() {
        // Requirement 4d: two handles append concurrently; seqs stay unique
        // and every entry lands.
        let (_d, m) = test_manager();
        let s = Arc::new(session(&m));
        let s1 = s.clone();
        let s2 = s.clone();
        let t1 = std::thread::spawn(move || {
            for i in 0..50u32 {
                // Even plan indexes (0..98); index 0 is the root step.
                let parent = if i == 0 { None } else { Some(2 * i - 1) };
                s1.ledger_plan_step_added(2 * i, &format!("a{i}"), parent)
                    .unwrap();
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..50u32 {
                s2.ledger_plan_step_added(2 * i + 1, &format!("b{i}"), Some(2 * i))
                    .unwrap();
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();
        let entries = collect_all(&s);
        assert_eq!(entries.len(), 100, "every concurrent append must land");
        let mut seqs: Vec<i64> = entries.iter().map(|e| e.seq).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 100, "seqs must be unique");
        let mut indexes: Vec<u32> = Vec::new();
        for e in &entries {
            if let LedgerPayload::PlanStepAdded { step_index, .. } = &e.payload {
                indexes.push(*step_index);
            }
        }
        indexes.sort_unstable();
        indexes.dedup();
        assert_eq!(indexes.len(), 100, "every planned step present");
        // The fold is coherent after the interleaving.
        let view = s.ledger_view().unwrap();
        assert_eq!(view.head.plan_steps.len(), 100);
    }

    #[test]
    fn crash_between_append_and_checkpoint_recovers_both_orders() {
        // Requirement 4e: a crash between an entry append and the head
        // checkpoint rebuilds the head from entries — in both orders.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // First "process": appends, NO head checkpoint yet, "crash".
        let (store, cas) = (root.join("store"), root.join("cas"));
        {
            let m = crate::SessionManager::open(&store, &cas, true).unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let s = m.create_session(ws, "t", "p", "m").unwrap();
            let id = s.id();
            s.ledger_goal_set("goal-batch-1").unwrap();
            s.ledger_decision("d", "c", "r").unwrap();
            assert!(
                m.store().ledger_head(id).unwrap().is_none(),
                "crash before the first checkpoint"
            );
            // Second "process" reopens and must rebuild the head.
            let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
            let s2 = m2.get_session(id).unwrap().unwrap();
            let head = m2.store().ledger_head(id).unwrap().unwrap();
            assert_eq!(head.checkpoint_seq, 2);
            let view = s2.ledger_view().unwrap();
            assert_eq!(view.head.goal, "goal-batch-1");
            // Third "process": head EXISTS, more appends land, crash again
            // before the checkpoint folds them.
            s2.ledger_decision("d2", "c2", "r2").unwrap();
            s2.ledger_routing_decision(2, "p", "m", "r", 1).unwrap();
            let m3 = crate::SessionManager::open(&store, &cas, true).unwrap();
            let s3 = m3.get_session(id).unwrap().unwrap();
            let view = s3.ledger_view().unwrap();
            assert_eq!(view.head.goal, "goal-batch-1");
            assert_eq!(view.head.decisions.len(), 2);
            assert_eq!(view.head.routing_count, 1);
            assert_eq!(
                m3.store().ledger_head(id).unwrap().unwrap().checkpoint_seq,
                4
            );
        }
    }

    #[test]
    fn corrupted_head_recovers_but_corrupted_entry_fails_the_open() {
        // Requirement 4f: a hand-corrupted head recovers by replay from
        // entries; an entry whose JSON fails its schema decode fails the
        // session open loudly — never silently dropped.
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = (dir.path().join("store"), dir.path().join("cas"));
        let id = {
            let m = crate::SessionManager::open(&store, &cas, true).unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let s = m.create_session(ws, "t", "p", "m").unwrap();
            s.ledger_goal_set("the-goal").unwrap();
            s.ledger_criteria_set(&["c".into()], "c").unwrap();
            s.ledger_decision("d", "c", "r").unwrap();
            s.ledger_blocker_opened("open-b").unwrap();
            // Head materialized.
            let _ = s.ledger_view().unwrap();
            // Hand-corrupt the head JSON.
            raw_sql(
                &m,
                &format!(
                    "UPDATE ledger_head SET head_json = 'garbage{{{{' WHERE session_id = {}",
                    s.id().raw()
                ),
            );
            s.id()
        };
        // Open recovers by replaying the surviving entries.
        let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
        let h2 = m2.get_session(id).unwrap().unwrap();
        let view = h2.ledger_view().unwrap();
        assert_eq!(view.head.goal, "the-goal");
        assert_eq!(
            view.head.open_blockers,
            vec!["open-b".to_string()],
            "entry replay must rebuild the full head"
        );
        assert_eq!(
            m2.store().ledger_head(id).unwrap().unwrap().checkpoint_seq,
            4
        );
        // Now corrupt an ENTRY payload: the next open fails loudly.
        h2.ledger_decision("d2", "c2", "r2").unwrap();
        let last_seq = m2.store().ledger_max_seq(id).unwrap();
        raw_sql(
            &m2,
            &format!(
                "UPDATE ledger_entry SET payload = 'not-json{{{{' WHERE session_id = {} AND seq = {last_seq}",
                id.raw()
            ),
        );
        let err = m2.get_session(id).unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "corrupt entry must fail the open loudly"
        );
        let err = h2.ledger_view().unwrap_err();
        assert!(err.to_string().contains("payload"), "{err}");
    }

    #[test]
    fn journal_replay_fails_loudly_on_crafted_future_payload_version() {
        // A journal event row crafted with payload_ver 999 fails the typed
        // replay loudly (never a silent v1 parse).
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("work", &[]).unwrap();
        raw_sql(
            &m,
            &format!(
                "UPDATE event SET payload_ver = 999 WHERE session_id = {} AND kind = 'prompt_received'",
                s.id().raw()
            ),
        );
        let err = s.replay_journal().unwrap_err();
        assert!(err.to_string().contains("999"), "{err}");
    }

    #[test]
    fn typed_ledger_is_per_session() {
        let (_d, m) = test_manager();
        let s1 = session(&m);
        let ws = m.create_workspace("/w2").unwrap();
        let s2 = m.create_session(ws, "t2", "p", "m").unwrap();
        s1.ledger_goal_set("one").unwrap();
        assert!(s2.ledger_view().unwrap().head.goal.is_empty());
        s2.ledger_goal_set("two").unwrap();
        assert_eq!(s1.ledger_view().unwrap().head.goal, "one");
        assert_eq!(s2.ledger_view().unwrap().head.goal, "two");
    }

    #[test]
    fn hostile_entry_bounds_are_rejected_before_write() {
        let (_d, m) = test_manager();
        let s = session(&m);
        assert!(s.ledger_goal_set("").is_err());
        assert!(s.ledger_goal_set(&"x".repeat(MAX_LEDGER_TEXT + 1)).is_err());
        assert!(s.ledger_blocker_opened("").is_err());
        assert!(s.ledger_decision("s", "", "r").is_err());
        assert!(s.ledger_verify_run(&[], "passed").is_err());
        assert!(s
            .ledger_verify_run(
                &[LedgerCheckRun {
                    id: "c".into(),
                    passed: true
                }],
                "typo"
            )
            .is_err());
        assert!(s.ledger_epoch_bumped(Some(3), 3).is_err());
        assert!(s.ledger_plan_step_added(300, "x", None).is_err());
        assert!(s.ledger_plan_step_added(4, "x", Some(4)).is_err());
        // None of the rejections wrote anything.
        assert_eq!(collect_all(&s).len(), 0);
    }

    // ---------------------------------------------- durable edit txn rows

    fn edit_txn_file(path: &str, content: &[u8]) -> EditTxnLedgerFile {
        EditTxnLedgerFile {
            path: path.to_string(),
            base_digest: digest64(content.len() as u64),
            base_bytes_len: content.len() as u64,
        }
    }

    /// A deterministic 64-hex digest shape (the ledger validates the shape,
    /// never the digest's truthfulness — that is the engine's CAS axis).
    fn digest64(seed: u64) -> String {
        format!("{seed:064x}")
    }

    #[test]
    fn edit_txn_roundtrip_and_open_set() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let f1 = edit_txn_file("a.txt", b"one");
        let f2 = edit_txn_file("b.txt", b"two");
        s.ledger_edit_txn_prepared(11, "run-1", &[f1.clone(), f2.clone()], "roll_forward")
            .unwrap();
        let open = s.ledger_open_edit_txns().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].txn_id, 11);
        assert_eq!(open[0].files, vec![f1.clone(), f2.clone()]);
        assert!(open[0].progress.is_empty());
        // Session open verification decodes the new rows (shape-strict).
        let view = s.ledger_view().unwrap();
        assert!(view.head.goal.is_empty(), "fold ignores edit txn rows");
        // Progress rows while open.
        s.ledger_edit_txn_progress(11, 0, "a.txt", "committed")
            .unwrap();
        s.ledger_edit_txn_progress(11, 1, "b.txt", "conflicted")
            .unwrap();
        let open = s.ledger_open_edit_txns().unwrap();
        assert_eq!(open[0].progress.len(), 2);
        assert_eq!(open[0].progress[1].outcome, "conflicted");
        // Terminal closes the transaction.
        s.ledger_edit_txn_committed(11, &["a.txt".to_string()], &["b.txt".to_string()], &[])
            .unwrap();
        assert!(s.ledger_open_edit_txns().unwrap().is_empty());
        // The typed stream keeps every row (strict decode, per session).
        let all = collect_all(&s);
        let kinds: Vec<&str> = all.iter().map(|e| e.entry_type.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "edit_txn_prepared",
                "edit_txn_progress",
                "edit_txn_progress",
                "edit_txn_committed"
            ]
        );
        // A second transaction with its own id is independent.
        s.ledger_edit_txn_prepared(12, "run-2", &[f1], "roll_back")
            .unwrap();
        let open = s.ledger_open_edit_txns().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].txn_id, 12);
        assert_eq!(open[0].strategy, "roll_back");
    }

    #[test]
    fn edit_txn_open_rows_survive_restart_durably() {
        // The recovery record is DURABLE: reopen the store and the open
        // transaction is still there, decoded.
        let dir = tempfile::tempdir().unwrap();
        let (store, cas) = (dir.path().join("store"), dir.path().join("cas"));
        let id = {
            let m = crate::SessionManager::open(&store, &cas, true).unwrap();
            let ws = m.create_workspace("/w").unwrap();
            let s = m.create_session(ws, "t", "p", "m").unwrap();
            s.ledger_edit_txn_prepared(
                21,
                "run-1",
                &[edit_txn_file("a.txt", b"one")],
                "roll_forward",
            )
            .unwrap();
            s.id()
        };
        let m2 = crate::SessionManager::open(&store, &cas, true).unwrap();
        let s2 = m2.get_session(id).unwrap().unwrap();
        let open = s2.ledger_open_edit_txns().unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].files[0].path, "a.txt");
    }

    #[test]
    fn edit_txn_compaction_pins_open_rows_and_prunes_closed_ones() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.ledger_goal_set("g").unwrap();
        // OPEN transaction: prepared + one progress row, no terminal.
        s.ledger_edit_txn_prepared(
            31,
            "run-1",
            &[
                edit_txn_file("a.txt", b"one"),
                edit_txn_file("b.txt", b"two"),
            ],
            "roll_forward",
        )
        .unwrap();
        s.ledger_edit_txn_progress(31, 0, "a.txt", "committed")
            .unwrap();
        // A CLOSED transaction, older in the stream.
        s.ledger_edit_txn_prepared(
            32,
            "run-2",
            &[edit_txn_file("c.txt", b"three")],
            "roll_back",
        )
        .unwrap();
        s.ledger_edit_txn_progress(32, 0, "c.txt", "committed")
            .unwrap();
        s.ledger_edit_txn_committed(32, &["c.txt".to_string()], &[], &[])
            .unwrap();
        // Compaction while 31 is OPEN: 31's rows must survive, 32's (closed)
        // may be pruned.
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.deleted > 0, "{report:?}");
        let all = collect_all(&s);
        let edit_rows: Vec<&TypedLedgerEntry> = all
            .iter()
            .filter(|e| {
                matches!(
                    &e.payload,
                    LedgerPayload::EditTxnPrepared { .. }
                        | LedgerPayload::EditTxnProgress { .. }
                        | LedgerPayload::EditTxnCommitted { .. }
                        | LedgerPayload::EditTxnRolledBack { .. }
                )
            })
            .collect();
        let open = s.ledger_open_edit_txns().unwrap();
        assert_eq!(open.len(), 1, "only txn 31 stays open");
        assert_eq!(open[0].txn_id, 31);
        assert_eq!(open[0].progress.len(), 1);
        assert!(
            edit_rows.iter().all(
                |e| matches!(&e.payload, LedgerPayload::EditTxnPrepared { txn_id, .. }
                    | LedgerPayload::EditTxnProgress { txn_id, .. } if *txn_id == 31)
            ),
            "compaction must prune closed txn rows and keep only open rows"
        );
        // Terminal row lands; the next compaction prunes the whole txn.
        s.ledger_edit_txn_committed(31, &["a.txt".to_string(), "b.txt".to_string()], &[], &[])
            .unwrap();
        s.compact_typed_ledger().unwrap();
        let all = collect_all(&s);
        assert!(
            !all.iter().any(|e| matches!(
                &e.payload,
                LedgerPayload::EditTxnPrepared { .. }
                    | LedgerPayload::EditTxnProgress { .. }
                    | LedgerPayload::EditTxnCommitted { .. }
                    | LedgerPayload::EditTxnRolledBack { .. }
            )),
            "closed edit txn rows age out of the stream"
        );
        assert!(s.ledger_open_edit_txns().unwrap().is_empty());
    }

    #[test]
    fn edit_txn_hostile_raw_store_rows_are_typed_and_never_silent() {
        // Craft rows DIRECTLY through the store (bypassing the typed
        // appenders) exactly like a hostile writer would. Every corruption
        // class must be a typed read error with NO open set returned, while
        // the session itself stays open. Each class gets a FRESH session:
        // a corrupt open txn stays corrupt for every read, by design.
        let (_d, m) = test_manager();
        let raw = |s: &SessionHandle, entry_type: &str, payload: serde_json::Value| {
            m.store()
                .append_ledger_entry(s.id(), entry_type, LEDGER_ENTRY_SCHEMA_V, payload)
                .unwrap();
        };
        let prepared_json = |txn: u64| {
            serde_json::json!({
                "kind": "edit_txn_prepared",
                "txn_id": txn,
                "session": "run-1",
                "files": [{"path": "a.txt", "base_digest": digest64(3), "base_bytes_len": 3}],
                "strategy": "roll_forward"
            })
        };
        let progress_json = |txn: u64, seq: u64, path: &str, outcome: &str| {
            serde_json::json!({
                "kind": "edit_txn_progress",
                "txn_id": txn,
                "seq": seq,
                "path": path,
                "outcome": outcome
            })
        };
        let committed_json = |txn: u64| {
            serde_json::json!({
                "kind": "edit_txn_committed",
                "txn_id": txn,
                "committed": ["a.txt"],
                "conflicted": [],
                "skipped": []
            })
        };
        // (f) hostile progress payload for a txn that was never prepared.
        let s = session(&m);
        raw(
            &s,
            "edit_txn_progress",
            progress_json(901, 0, "a.txt", "committed"),
        );
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("orphaned"), "{err}");
        // The session itself still opens and its other views are fine.
        assert!(m.get_session(s.id()).is_ok());
        assert!(s.ledger_view().is_ok());
        assert_eq!(collect_all(&s).len(), 1, "nothing was silently dropped");
        // Prepared + a progress row that DECODES but has a garbage outcome.
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(902));
        raw(
            &s,
            "edit_txn_progress",
            progress_json(902, 0, "a.txt", "sideways"),
        );
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("sideways"), "{err}");
        // Out-of-range seq against a one-file prepared row.
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(902));
        raw(
            &s,
            "edit_txn_progress",
            progress_json(902, 7, "a.txt", "committed"),
        );
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
        // Duplicate progress seq.
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(903));
        raw(
            &s,
            "edit_txn_progress",
            progress_json(903, 0, "a.txt", "committed"),
        );
        raw(
            &s,
            "edit_txn_progress",
            progress_json(903, 0, "a.txt", "conflicted"),
        );
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("duplicate progress seq"), "{err}");
        // Progress path that does not match the prepared file at that seq.
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(904));
        raw(
            &s,
            "edit_txn_progress",
            progress_json(904, 0, "other.txt", "committed"),
        );
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(
            err.to_string().contains("does not match prepared file"),
            "{err}"
        );
        // Duplicate prepared for one txn id (a re-begin is corruption).
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(905));
        raw(&s, "edit_txn_prepared", prepared_json(905));
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("duplicate prepared"), "{err}");
        // A second terminal for one txn id.
        let s = session(&m);
        raw(&s, "edit_txn_prepared", prepared_json(906));
        raw(&s, "edit_txn_committed", committed_json(906));
        raw(&s, "edit_txn_committed", committed_json(906));
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("duplicate terminal"), "{err}");
        // Terminal row without a prepared row.
        let s = session(&m);
        raw(&s, "edit_txn_committed", committed_json(907));
        let err = s.ledger_open_edit_txns().unwrap_err();
        assert!(err.to_string().contains("unknown edit txn"), "{err}");
        // In every hostile case the session stays open (nothing failed
        // its shape decode) and the crafted row is still there.
        assert!(m.get_session(s.id()).is_ok());
    }

    #[test]
    fn edit_txn_appender_bounds_reject_before_journaling() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let ok_file = edit_txn_file("a.txt", b"one");
        // Zero / oversize / malformed inputs.
        assert!(s
            .ledger_edit_txn_prepared(0, "s", std::slice::from_ref(&ok_file), "roll_forward")
            .is_err());
        assert!(s
            .ledger_edit_txn_prepared(1, "", std::slice::from_ref(&ok_file), "roll_forward")
            .is_err());
        assert!(s
            .ledger_edit_txn_prepared(1, "s", &[], "roll_forward")
            .is_err());
        assert!(s
            .ledger_edit_txn_prepared(1, "s", std::slice::from_ref(&ok_file), "sideways")
            .is_err());
        let mut bad_digest = ok_file.clone();
        bad_digest.base_digest = "zz".repeat(32);
        assert!(s
            .ledger_edit_txn_prepared(1, "s", &[bad_digest], "roll_forward")
            .is_err());
        let mut long_path = ok_file.clone();
        long_path.path = "x".repeat(MAX_LEDGER_TEXT + 1);
        assert!(s
            .ledger_edit_txn_prepared(1, "s", &[long_path], "roll_forward")
            .is_err());
        // File-count bound: > MAX_EDIT_TXN_FILES files is refused.
        let many = vec![ok_file.clone(); MAX_EDIT_TXN_FILES + 1];
        let err = s
            .ledger_edit_txn_prepared(1, "s", &many, "roll_forward")
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
        // Payload bound: one prepared row whose JSON exceeds the entry cap
        // is refused by the shared append tail (nothing journaled).
        let wide: Vec<EditTxnLedgerFile> = (0..400)
            .map(|i| EditTxnLedgerFile {
                path: format!("dir-{i}/{}", "f".repeat(120)),
                base_digest: ok_file.base_digest.clone(),
                base_bytes_len: 3,
            })
            .collect();
        let err = s
            .ledger_edit_txn_prepared(1, "s", &wide, "roll_forward")
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
        // Progress / terminal shape bounds.
        assert!(s
            .ledger_edit_txn_progress(0, 0, "a.txt", "committed")
            .is_err());
        assert!(s.ledger_edit_txn_progress(1, 0, "", "committed").is_err());
        assert!(s.ledger_edit_txn_progress(1, 0, "a.txt", "maybe").is_err());
        assert!(s
            .ledger_edit_txn_committed(0, &["a".into()], &[], &[])
            .is_err());
        assert!(s
            .ledger_edit_txn_committed(1, &["".into()], &[], &[])
            .is_err());
        assert!(s
            .ledger_edit_txn_rolled_back(0, &["a".into()], &[])
            .is_err());
        assert!(s
            .ledger_edit_txn_rolled_back(1, &["a".into()], &["x".repeat(MAX_LEDGER_TEXT + 1)])
            .is_err());
        assert_eq!(collect_all(&s).len(), 0, "no hostile input was journaled");
    }

    /// Adversarial learning-record surface (audits 65-67/92): strict shape
    /// bounds at append AND decode, verbatim payload round-trip, pinning
    /// across watermark compaction, and a raw hostile row failing the read
    /// loudly instead of being dropped.
    #[test]
    fn learning_records_roundtrip_pin_across_compaction_and_fail_loud() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let episode_seq = s
            .ledger_learning_record(LEARNING_RECORD_EPISODE, r#"{"episode":1}"#)
            .unwrap()
            .unwrap();
        let learning_seq = s
            .ledger_learning_record(LEARNING_RECORD_LEARNING, r#"{"learning":2}"#)
            .unwrap()
            .unwrap();
        let rows = s.ledger_learning_records().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, episode_seq);
        assert_eq!(rows[0].record, LEARNING_RECORD_EPISODE);
        assert_eq!(rows[0].payload, r#"{"episode":1}"#);
        assert_eq!(rows[1].record, LEARNING_RECORD_LEARNING);

        // Shape bounds are enforced before anything is journaled: an
        // unknown kind, an empty payload and an oversized payload are all
        // loud typed errors.
        assert!(s.ledger_learning_record("bogus", "{}").is_err());
        assert!(s
            .ledger_learning_record(LEARNING_RECORD_LEARNING, "")
            .is_err());
        let oversized = "x".repeat(MAX_LEARNING_RECORD_PAYLOAD + 1);
        let err = s
            .ledger_learning_record(LEARNING_RECORD_LEARNING, &oversized)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
        assert_eq!(s.ledger_learning_records().unwrap().len(), 2);

        // Watermark compaction pins learning corpus rows: a mined learning
        // must never be silently aged out with turn history.
        s.ledger_goal_set("keep me").unwrap();
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.pinned.contains(&episode_seq));
        assert!(report.pinned.contains(&learning_seq));
        assert_eq!(
            s.ledger_learning_records().unwrap().len(),
            2,
            "learning rows survive compaction"
        );

        // A raw hostile row (unknown record kind, bypassing the appender)
        // must fail every strict read and the session-open verification —
        // never be silently skipped.
        m.store()
            .append_ledger_entry(
                s.id(),
                ENTRY_LEARNING_RECORD,
                LEDGER_ENTRY_SCHEMA_V,
                serde_json::json!({
                    "kind": "learning_record",
                    "record": "bogus",
                    "payload": "{}",
                }),
            )
            .unwrap();
        let err = s.ledger_learning_records().unwrap_err();
        assert!(
            err.to_string().contains("episode|learning|removed"),
            "{err}"
        );
        assert!(
            s.ledger_verify_open().is_err(),
            "corrupt row fails the open"
        );

        // A valid-shape row whose payload is semantically corrupt is still
        // decodable by THIS layer (shape only); the learning adapter is the
        // strict payload decoder and is covered in faktor-learning.
        let s2 = session(&m);
        s2.ledger_learning_record(LEARNING_RECORD_LEARNING, "not json")
            .unwrap();
        let rows = s2.ledger_learning_records().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].payload, "not json");
    }

    /// Adversarial completion-contract ledger surface (P2): immutability per
    /// `(task, revision)`, latest-contract-wins across revisions, pinning
    /// across watermark compaction, strict reopen, and hostile raw rows
    /// failing every typed read loudly instead of being dropped.
    #[test]
    fn completion_contract_rows_are_immutable_pinned_and_reopen_stable() {
        let (dir, m) = test_manager();
        let s = session(&m);
        let sid = s.id;
        let contract_v3 = CompletionContract {
            include_commit: true,
            include_push: true,
            include_pr: false,
        };
        let contract_v4 = CompletionContract {
            include_commit: false,
            include_push: false,
            include_pr: true,
        };
        let seq_v3 = s
            .ledger_completion_contract_set(7, 3, &contract_v3)
            .unwrap()
            .unwrap();
        // Immutable per (task, revision).
        let err = s
            .ledger_completion_contract_set(7, 3, &contract_v4)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
        // A later revision is a new run: accepted and the newest row wins.
        let seq_v4 = s
            .ledger_completion_contract_set(7, 4, &contract_v4)
            .unwrap()
            .unwrap();
        let latest = s.ledger_completion_contract(7).unwrap().unwrap();
        assert_eq!(latest.revision, 4);
        assert_eq!(latest.contract, contract_v4);
        assert!(latest.seq > seq_v3);
        // The exact row of an older revision is still addressable.
        let old = s.ledger_completion_contract_at(7, 3).unwrap().unwrap();
        assert_eq!(old.contract, contract_v3);
        // Step statuses require the recorded contract revision.
        let status_seq = s
            .ledger_completion_step_status(
                7,
                4,
                CompletionStep::Pr,
                CompletionStepOutcome::Succeeded,
                "opened PR #12",
                99,
            )
            .unwrap()
            .unwrap();
        let err = s
            .ledger_completion_step_status(
                7,
                9,
                CompletionStep::Pr,
                CompletionStepOutcome::Succeeded,
                "orphan",
                99,
            )
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
        let rows = s.ledger_completion_step_statuses(7, 4).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].step, CompletionStep::Pr);
        assert_eq!(rows[0].status, CompletionStepOutcome::Succeeded);
        assert_eq!(rows[0].detail, "opened PR #12");
        assert_eq!(rows[0].at_ms, 99);

        // Watermark compaction pins contract + status rows.
        s.ledger_goal_set("compaction pressure").unwrap();
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.pinned.contains(&seq_v3));
        assert!(report.pinned.contains(&seq_v4));
        assert!(report.pinned.contains(&status_seq));
        assert!(collect_all(&s).iter().any(|e| matches!(
            &e.payload,
            LedgerPayload::CompletionContractSet { revision: 3, .. }
        )));

        // Reopen the exact store: the strict open verifies every row and the
        // reads converge byte-identically.
        drop(s);
        drop(m);
        let m2 =
            crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                .unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        assert_eq!(
            s2.ledger_completion_contract(7).unwrap().unwrap().contract,
            contract_v4
        );
        assert_eq!(
            s2.ledger_completion_contract_at(7, 3)
                .unwrap()
                .unwrap()
                .contract,
            contract_v3
        );
        assert_eq!(s2.ledger_completion_step_statuses(7, 4).unwrap().len(), 1);
    }

    /// FIX 2: the explicit durable-read distinction is never collapsed — a
    /// missing row, a present valid row, a present corrupt row and a failed
    /// store read are four distinct outcomes on every classified read.
    #[test]
    fn durable_read_distinguishes_missing_valid_malformed_and_store_failure() {
        let (_d, m) = test_manager();
        let s = session(&m);
        // (1) Missing: nothing was written for these identities.
        assert!(s.ledger_completion_contract_read(7).is_missing());
        assert!(s.ledger_run_base_read("run-x").is_missing());
        assert!(s.ledger_integration_txn_read("run-x").is_missing());
        assert!(s.ledger_integration_record_for_task_read(7).is_missing());
        // (2) PresentValid: the typed appenders land decodable rows.
        let contract = CompletionContract {
            include_commit: true,
            include_push: false,
            include_pr: false,
        };
        s.ledger_completion_contract_set(7, 3, &contract)
            .unwrap()
            .unwrap();
        assert!(matches!(
            s.ledger_completion_contract_read(7),
            DurableRead::PresentValid(ref row) if row.contract == contract
        ));
        s.ledger_run_base_set(&RunBaseRecord {
            run_id: "run-x".into(),
            workspace_id: 1,
            worktree_id: 1,
            snapshot_hash: format!("tm1:{}", "a".repeat(64)),
            manifest_digest: "b".repeat(64),
            root: "/base".into(),
            created_ms: 1,
        })
        .unwrap();
        assert!(matches!(
            s.ledger_run_base_read("run-x"),
            DurableRead::PresentValid(ref row) if row.run_id == "run-x"
        ));
        // (3) PresentMalformed: a raw all-false contract row (bypassing the
        // typed appender) is CORRUPTION on read — never "no contract".
        let s2 = session(&m);
        m.store()
            .append_ledger_entry(
                s2.id,
                ENTRY_COMPLETION_CONTRACT_SET,
                LEDGER_ENTRY_SCHEMA_V,
                serde_json::json!({
                    "kind": "completion_contract_set",
                    "task_id": 9,
                    "revision": 1,
                    "contract": {
                        "include_commit": false,
                        "include_push": false,
                        "include_pr": false,
                    },
                }),
            )
            .unwrap();
        assert!(
            s2.ledger_completion_contract_read(9).is_present_malformed(),
            "a corrupt contract row must classify as PresentMalformed"
        );
        // (4) StoreFailure: with the ledger table gone the SAME read is a
        // failed store read — an error, never Missing and never "valid".
        m.store().sql_execute("DROP TABLE ledger_entry").unwrap();
        assert!(matches!(
            s2.ledger_completion_contract_read(9),
            DurableRead::StoreFailure(_)
        ));
        assert!(matches!(
            s2.ledger_run_base_read("run-x"),
            DurableRead::StoreFailure(_)
        ));
    }

    /// Hostile completion rows fail the strict decode and the session-open
    /// verification: an all-false contract row, an unknown step tag, an
    /// unknown status tag, a non-positive `at_ms`, and an oversized detail.
    #[test]
    fn hostile_completion_rows_fail_loudly() {
        let (_d, m) = test_manager();
        let raw = |s: &SessionHandle, entry_type: &str, json: serde_json::Value| {
            m.store()
                .append_ledger_entry(s.id, entry_type, LEDGER_ENTRY_SCHEMA_V, json)
                .unwrap();
        };
        // The appender refuses an all-false contract before journaling.
        let s = session(&m);
        assert!(s
            .ledger_completion_contract_set(1, 1, &CompletionContract::default())
            .is_err());
        assert_eq!(collect_all(&s).len(), 0);
        // A raw all-false contract row is corruption: loud on read + open.
        raw(
            &s,
            ENTRY_COMPLETION_CONTRACT_SET,
            serde_json::json!({
                "kind": "completion_contract_set",
                "task_id": 1,
                "revision": 1,
                "contract": {
                    "include_commit": false,
                    "include_push": false,
                    "include_pr": false,
                },
            }),
        );
        let err = s.ledger_completion_contract(1).unwrap_err();
        assert!(err.to_string().contains("all-false"), "{err}");
        assert!(s.ledger_verify_open().is_err());

        // Unknown step tag cannot even decode.
        let s = session(&m);
        raw(
            &s,
            ENTRY_COMPLETION_STEP_STATUS,
            serde_json::json!({
                "kind": "completion_step_status",
                "task_id": 1,
                "revision": 1,
                "step": "deploy",
                "status": "succeeded",
                "detail": "",
                "at_ms": 1,
            }),
        );
        let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
        assert!(err.to_string().contains("schema"), "{err}");

        // Unknown status tag.
        let s = session(&m);
        raw(
            &s,
            ENTRY_COMPLETION_STEP_STATUS,
            serde_json::json!({
                "kind": "completion_step_status",
                "task_id": 1,
                "revision": 1,
                "step": "push",
                "status": "maybe",
                "detail": "",
                "at_ms": 1,
            }),
        );
        assert!(s.ledger_completion_step_statuses(1, 1).is_err());

        // Non-positive at_ms.
        let s = session(&m);
        raw(
            &s,
            ENTRY_COMPLETION_STEP_STATUS,
            serde_json::json!({
                "kind": "completion_step_status",
                "task_id": 1,
                "revision": 1,
                "step": "push",
                "status": "succeeded",
                "detail": "",
                "at_ms": 0,
            }),
        );
        let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
        assert!(err.to_string().contains("at_ms"), "{err}");

        // Oversized detail.
        let s = session(&m);
        raw(
            &s,
            ENTRY_COMPLETION_STEP_STATUS,
            serde_json::json!({
                "kind": "completion_step_status",
                "task_id": 1,
                "revision": 1,
                "step": "push",
                "status": "succeeded",
                "detail": "x".repeat(MAX_COMPLETION_STEP_DETAIL + 1),
                "at_ms": 1,
            }),
        );
        let err = s.ledger_completion_step_statuses(1, 1).unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Oversized, "{err}");
    }

    /// Hardening: the explicit integration-identity fields
    /// (`run_base_snapshot`/`candidate_snapshot`/`landed_snapshot`/
    /// `proof_basis_digest`/`integration_txn_id`) round-trip a real reopen
    /// byte-identically, and the txn id is the same before and after.
    #[test]
    fn integration_explicit_identity_fields_survive_reopen() {
        use faktor_core::id::SessionId as Sid;
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store");
        let cas = dir.path().join("cas");
        let m = crate::SessionManager::open(store.clone(), cas.clone(), true).unwrap();
        let s = session(&m);
        let sid: Sid = s.id;
        let hex = |c: char| c.to_string().repeat(64);
        let txn = IntegrationTxnRow {
            run_id: "run-identity".into(),
            task_id: 9,
            owner_root: "/owner".into(),
            candidate_root: "/candidate".into(),
            run_base_snapshot: hex('1'),
            verified_candidate_snapshot: hex('2'),
            sources_digest: hex('3'),
            phase: IntegrationTxnPhase::Landed,
            paths: Vec::new(),
            path_count: 0,
            applied_count: 0,
            conflicts: Vec::new(),
            at_ms: 5,
        };
        let txn_id = txn.txn_id();
        assert!(txn_id.starts_with("blake3:"));
        assert_eq!(txn_id, txn.txn_id(), "content identity is stable");
        s.ledger_integration_txn_set(&txn).unwrap();
        let record = IntegrationRecordRow {
            run_id: "run-identity".into(),
            task_id: 9,
            base_revision: None,
            base_snapshot: Some(hex('1')),
            run_base_snapshot: Some(hex('1')),
            candidate_snapshot: Some(hex('2')),
            landed_snapshot: Some(hex('4')),
            proof_basis_digest: Some(format!("blake3:{}", hex('5'))),
            integration_txn_id: Some(txn_id.clone()),
            final_root: "/owner".into(),
            final_snapshot_hash: hex('4'),
            integrated_files: vec!["src/lib.rs".into()],
            integrated_file_count: 1,
            integrated_files_digest: hex('6'),
            conflicts: Vec::new(),
            conflict_count: 0,
            sources: Vec::new(),
            source_count: 0,
            sources_digest: String::new(),
            at_ms: 6,
        };
        s.ledger_integration_record_set(&record).unwrap();
        assert_eq!(
            s.ledger_integration_record_for_task(9).unwrap().unwrap(),
            record
        );
        drop(s);
        drop(m);
        let m2 = crate::SessionManager::open(store, cas, true).unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        assert_eq!(
            s2.ledger_integration_record_for_task(9).unwrap().unwrap(),
            record,
            "explicit identity fields must survive a real reopen"
        );
        let txn2 = s2.ledger_integration_txn_for_run("run-identity").unwrap();
        assert_eq!(txn2.unwrap().txn_id(), txn_id, "txn id survives reopen");
    }

    /// Hardening: a tampered raw row that disagrees between the deprecated
    /// alias and the explicit field (or binds a hostile digest) is a typed
    /// read error — never a silently accepted identity.
    #[test]
    fn hostile_integration_identity_rows_fail_loudly() {
        let (_d, m) = test_manager();
        let raw = |s: &SessionHandle, record: serde_json::Value| {
            m.store()
                .append_ledger_entry(
                    s.id(),
                    ENTRY_INTEGRATION_RECORD,
                    LEDGER_ENTRY_SCHEMA_V,
                    serde_json::json!({ "kind": "integration_recorded", "record": record }),
                )
                .unwrap();
        };
        let base = |record: serde_json::Value| {
            let mut v = serde_json::json!({
                "run_id": "run-hostile",
                "task_id": 1,
                "final_root": "/owner",
                "final_snapshot_hash": "44".repeat(32),
                "integrated_files": [],
                "integrated_file_count": 0,
                "integrated_files_digest": "",
                "conflicts": [],
                "conflict_count": 0,
                "sources": [],
                "source_count": 0,
                "sources_digest": "",
                "at_ms": 1,
            });
            for (k, val) in record.as_object().unwrap() {
                v[k.as_str()] = val.clone();
            }
            v
        };
        // Deprecated alias and explicit run-base field disagree.
        let s = session(&m);
        raw(
            &s,
            base(serde_json::json!({
                "base_snapshot": "11".repeat(32),
                "run_base_snapshot": "22".repeat(32),
            })),
        );
        let err = s.ledger_integration_record_for_task(1).unwrap_err();
        assert!(err.to_string().contains("disagree"), "{err}");
        // Landed snapshot and final hash disagree.
        let s = session(&m);
        raw(
            &s,
            base(serde_json::json!({
                "run_base_snapshot": "11".repeat(32),
                "landed_snapshot": "22".repeat(32),
            })),
        );
        let err = s.ledger_integration_record_for_task(1).unwrap_err();
        assert!(err.to_string().contains("disagree"), "{err}");
        // Hostile non-hex explicit digest.
        let s = session(&m);
        raw(
            &s,
            base(serde_json::json!({ "candidate_snapshot": "not-hex" })),
        );
        assert!(s.ledger_integration_record_for_task(1).is_err());
        // Legacy row WITHOUT the new fields decodes additively (never a
        // missing-field failure).
        let s = session(&m);
        raw(&s, base(serde_json::json!({})));
        let legacy = s.ledger_integration_record_for_task(1).unwrap().unwrap();
        assert!(legacy.run_base_snapshot.is_none());
        assert!(legacy.candidate_snapshot.is_none());
        assert!(legacy.landed_snapshot.is_none());
        assert!(legacy.proof_basis_digest.is_none());
        assert!(legacy.integration_txn_id.is_none());
    }

    // -------------------------------------------------------- terminal rows

    fn terminal_row(terminal_id: &str) -> TerminalDurableRow {
        TerminalDurableRow {
            terminal_id: terminal_id.to_string(),
            session_id: 1,
            task_id: 1,
            agent_id: None,
            operation_id: 7,
            pid: 4242,
            start_time_ms: 1_700_000_000_000,
            at_ms: 1_700_000_000_100,
            execution_profile: String::new(),
        }
    }

    #[test]
    fn terminal_lifecycle_rows_round_trip_and_survive_compaction() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let row = terminal_row("7f1a2b3c-0000-4000-8000-000000000001");
        s.ledger_terminal_created(&row).unwrap();
        s.ledger_terminal_running(&row).unwrap();
        let exited = TerminalDurableRow {
            at_ms: row.at_ms + 10,
            ..row.clone()
        };
        s.ledger_terminal_exited(&exited, Some(0)).unwrap();

        // A hostile consumer cannot forge an illegal reconcile disposition.
        let err = s
            .ledger_terminal_reconciled(&exited, "obliterated")
            .unwrap_err();
        assert!(err.to_string().contains("killed|collected"), "{err}");

        let records = s.ledger_terminal_rows(None).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].kind, TerminalEventKind::Created);
        assert_eq!(records[1].kind, TerminalEventKind::Running);
        assert_eq!(records[2].kind, TerminalEventKind::Exited);
        assert_eq!(records[2].exit_code, Some(0));
        assert_eq!(records[2].row.pid, 4242);
        assert_eq!(records[2].row.start_time_ms, 1_700_000_000_000);

        // The terminal stream is pinned across compaction: turn history is
        // pruned, every terminal row survives byte-for-byte.
        for index in 0..40 {
            turn_entries(&s, index + 1);
        }
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.deleted > 0, "{report:?}");
        let after = s.ledger_terminal_rows(None).unwrap();
        assert_eq!(after.len(), 3, "terminal rows are never evicted");
        assert_eq!(after[0].row.terminal_id, row.terminal_id);
        assert_eq!(after[2].kind, TerminalEventKind::Exited);
    }

    #[test]
    fn terminal_execution_profile_round_trips_durably() {
        // The effective execution profile is a durable row fact: it survives
        // the round trip byte-for-byte (a legacy empty profile stays legal).
        let (_d, m) = test_manager();
        let s = session(&m);
        let mut profiled = terminal_row("7f1a2b3c-0000-4000-8000-00000000000f");
        profiled.execution_profile = "{\"cwd\":\"/tmp\",\"capabilities\":\"*\"}".into();
        s.ledger_terminal_created(&profiled).unwrap();
        let records = s.ledger_terminal_rows(None).unwrap();
        let stored = records
            .iter()
            .find(|record| record.row.terminal_id == profiled.terminal_id)
            .expect("profiled row");
        assert_eq!(stored.row.execution_profile, profiled.execution_profile);
    }

    #[test]
    fn terminal_row_shape_violations_are_loud_and_never_parse() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let mut zero_pid = terminal_row("7f1a2b3c-0000-4000-8000-000000000002");
        zero_pid.pid = 0;
        assert!(s.ledger_terminal_created(&zero_pid).is_err());
        let mut hostile_id = terminal_row("7f1a2b3c-0000-4000-8000-000000000003");
        hostile_id.terminal_id = "a".repeat(MAX_TERMINAL_ID_BYTES + 1);
        assert!(s.ledger_terminal_running(&hostile_id).is_err());
        let mut slash = terminal_row("bad/id");
        slash.terminal_id = "bad/id".into();
        assert!(s.ledger_terminal_created(&slash).is_err());
        let mut oversized_profile = terminal_row("7f1a2b3c-0000-4000-8000-000000000005");
        oversized_profile.execution_profile = "p".repeat(MAX_TERMINAL_PROFILE_BYTES + 1);
        assert!(s.ledger_terminal_created(&oversized_profile).is_err());
        let mut nul_profile = terminal_row("7f1a2b3c-0000-4000-8000-000000000006");
        nul_profile.execution_profile = "profile\0evil".into();
        assert!(s.ledger_terminal_created(&nul_profile).is_err());
        // Nothing was journaled by the refusals.
        assert!(s.ledger_terminal_rows(None).unwrap().is_empty());

        // A raw-store hostile row (valid JSON, wrong shape) fails the strict
        // decode loudly on read instead of being treated as absent.
        s.ledger_terminal_created(&terminal_row("7f1a2b3c-0000-4000-8000-000000000004"))
            .unwrap();
        raw_sql(
            &m,
            "UPDATE ledger_entry SET payload = json('{\"kind\":\"terminal_created\"}') \
             WHERE entry_type = 'terminal_created'",
        );
        let err = s.ledger_terminal_rows(None).unwrap_err();
        assert!(err.to_string().contains("schema"), "{err}");
    }

    // ------------------------------------------- external-operation identity

    const EXTERNAL_OP_KEY: &str = "task:1:rev:1:github:pull_request";

    fn external_operation_row(
        key: &str,
        head: &str,
        state: ExternalOperationState,
    ) -> ExternalOperationRow {
        let input = ExternalOperationInput {
            organization: "acme".into(),
            repository: "widgets".into(),
            head: head.into(),
            base: "main".into(),
            marker: "faktor:task:1:rev:1".into(),
        };
        let completed = state == ExternalOperationState::Completed;
        ExternalOperationRow {
            id: ExternalOperationRow::content_id(key, "github", "pull_request", &input),
            operation_key: key.into(),
            provider: "github".into(),
            kind: "pull_request".into(),
            input,
            state,
            remote_object_id: completed.then(|| "pr-1".into()),
            remote_object_version: completed.then(|| "v1".into()),
            started_at: 7,
            reconciled_at: None,
        }
    }

    #[test]
    fn external_operation_rows_round_trip_and_latest_state_wins() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let prepared =
            external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
        s.ledger_external_operation_set(&prepared).unwrap();
        assert!(s.ledger_external_operation_read("other-key").is_missing());
        assert_eq!(
            s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
            Some(prepared.clone())
        );
        let mut completed = prepared.clone();
        completed.state = ExternalOperationState::Completed;
        completed.remote_object_id = Some("pr-1".into());
        completed.remote_object_version = Some("v1".into());
        completed.reconciled_at = Some(8);
        s.ledger_external_operation_set(&completed).unwrap();
        assert_eq!(
            s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
            Some(completed.clone()),
            "the latest row of one operation key wins"
        );
        assert_eq!(
            s.ledger_external_operations().unwrap(),
            vec![prepared, completed]
        );
        // A hostile raw row is a LOUD read failure, never "no operation".
        raw_sql(
            &m,
            "UPDATE ledger_entry SET payload = json('{\"kind\":\"external_operation_recorded\"}') \
             WHERE entry_type = 'external_operation'",
        );
        assert!(s
            .ledger_external_operations()
            .unwrap_err()
            .to_string()
            .contains("schema"));
        assert!(s
            .ledger_external_operation_read(EXTERNAL_OP_KEY)
            .is_present_malformed());
    }

    #[test]
    fn external_operation_rows_refuse_shape_violations() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let prepared =
            external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
        // A hand-mismatched deterministic id.
        let mut tampered = prepared.clone();
        tampered.id = format!("blake3:{}", "0".repeat(64));
        assert!(s.ledger_external_operation_set(&tampered).is_err());
        // A prepared row may not carry a remote identity.
        let mut bogus = prepared.clone();
        bogus.remote_object_id = Some("pr-1".into());
        assert!(s.ledger_external_operation_set(&bogus).is_err());
        // A completed row must carry both remote id and version.
        let mut incomplete = prepared.clone();
        incomplete.state = ExternalOperationState::Completed;
        assert!(s.ledger_external_operation_set(&incomplete).is_err());
        // reconciled_at may not precede started_at.
        let mut backwards = prepared.clone();
        backwards.state = ExternalOperationState::Failed;
        backwards.reconciled_at = Some(1);
        assert!(s.ledger_external_operation_set(&backwards).is_err());
        // Control characters are refused.
        let mut hostile = prepared.clone();
        hostile.input.head = "main\n".into();
        assert!(s.ledger_external_operation_set(&hostile).is_err());
        // A different head is a DIFFERENT row (the operation key conflict is
        // resolved by the caller, never by an id collision).
        let other = external_operation_row(
            EXTERNAL_OP_KEY,
            "other-branch",
            ExternalOperationState::Prepared,
        );
        assert_ne!(prepared.id, other.id);
        // Nothing was journaled by the refusals.
        assert!(s.ledger_external_operations().unwrap().is_empty());
    }

    #[test]
    fn external_operation_rows_survive_compaction() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let prepared =
            external_operation_row(EXTERNAL_OP_KEY, "main", ExternalOperationState::Prepared);
        s.ledger_external_operation_set(&prepared).unwrap();
        // Unpinned rows below the watermark so the compaction has victims.
        for i in 0..5 {
            s.ledger_failure_recorded(&format!("failure-{i}")).unwrap();
        }
        let mut completed = prepared.clone();
        completed.state = ExternalOperationState::Completed;
        completed.remote_object_id = Some("pr-1".into());
        completed.remote_object_version = Some("v1".into());
        s.ledger_external_operation_set(&completed).unwrap();
        let report = s.compact_typed_ledger().unwrap();
        assert!(report.deleted > 0, "{report:?}");
        assert_eq!(
            s.ledger_external_operation_read(EXTERNAL_OP_KEY).valid(),
            Some(completed),
            "the reconciliation authority outlives compaction"
        );
    }
}
