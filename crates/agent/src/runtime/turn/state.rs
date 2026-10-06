//! `runtime::turn::state`: cohesive slice of the turn module.

use super::*;

/// Default stall-silence budget (see [`StallTracker`]): total silence
/// (no output, no progress, no op completion) past this marks the session
/// stalled. Tunable per runtime via
/// [`AgentRuntime::set_stall_silence_ms`]; 0 disables time-stall detection.
pub const DEFAULT_STALL_SILENCE_MS: u64 = 10 * 60 * 1000;

/// Per-envelope backing cap of the runtime's durable evidence authority:
/// normalized/compressed output up to this many bytes stays retrievable
/// from the CAS; larger backing is recorded by digest and dropped (the
/// envelope survives, retrieval fails loudly — bounded everything).
pub const EVIDENCE_BACKING_CAP_BYTES: usize = 8 * 1024 * 1024;

// ------------------------------------------------- typed child handoff
//
// The parent consumes a child's BOUNDED typed handoff — durable facts,
// findings, decisions, changed files and scoped refs to the backing rows —
// never the child's transcript. A child that read 100k tokens of evidence
// contributes at most the configured budget; every omitted backing stays
// retrievable through the refs (`session:<id>` / `child:<id>`) against the
// durable rows, so information is deferred, never destroyed.

/// Default parent-imposed handoff budget in tokens (the orchestrator/parent
/// may lower or raise it per delegation; the render never exceeds it).
pub const DEFAULT_CHILD_HANDOFF_TOKENS: usize = 2_048;

/// Hard cap of rendered handoff items (bounded everything).
pub const MAX_CHILD_HANDOFF_ITEMS: usize = 16;

/// Per-item render cap in chars: each fact/finding/decision/file/ref line
/// is truncated at the head so one hostile item can never dominate the
/// budget.
pub(crate) const CHILD_HANDOFF_ITEM_CHARS: usize = 240;

/// The bounded typed handoff of ONE child agent (audit: the parent consumes
/// THIS, never the child transcript). `refs` are scoped pointers to the
/// backing rows; everything not rendered here remains retrievable through
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChildHandoff {
    pub child_id: String,
    pub child_session: Option<SessionId>,
    pub goal: String,
    pub outcome: String,
    pub facts: Vec<String>,
    pub findings: Vec<String>,
    pub decisions: Vec<String>,
    pub changed_files: Vec<String>,
    pub refs: Vec<String>,
}

impl ChildHandoff {
    pub fn new(child_id: impl Into<String>) -> Self {
        Self {
            child_id: child_id.into(),
            ..Default::default()
        }
    }

    pub(crate) fn item_line(label: &str, items: &[String]) -> String {
        if items.is_empty() {
            return String::new();
        }
        let mut out = format!("{label}:\n");
        for item in items.iter().take(MAX_CHILD_HANDOFF_ITEMS) {
            out.push_str(&format!("- {}\n", truncate(item, CHILD_HANDOFF_ITEM_CHARS)));
        }
        out
    }

    pub(crate) fn render_unbounded(&self) -> String {
        let mut out = String::from("[child-handoff]\n");
        if !self.child_id.is_empty() {
            out.push_str(&format!(
                "child: {}\n",
                truncate(&self.child_id, CHILD_HANDOFF_ITEM_CHARS)
            ));
        }
        if !self.goal.is_empty() {
            out.push_str(&format!(
                "goal: {}\n",
                truncate(&self.goal, CHILD_HANDOFF_ITEM_CHARS)
            ));
        }
        if !self.outcome.is_empty() {
            out.push_str(&format!(
                "outcome: {}\n",
                truncate(&self.outcome, CHILD_HANDOFF_ITEM_CHARS)
            ));
        }
        out.push_str(&Self::item_line("facts", &self.facts));
        out.push_str(&Self::item_line("findings", &self.findings));
        out.push_str(&Self::item_line("decisions", &self.decisions));
        if !self.changed_files.is_empty() {
            out.push_str("changed files: ");
            out.push_str(&truncate(&self.changed_files.join(", "), 300));
            out.push('\n');
        }
        out.push_str(&Self::item_line("refs", &self.refs));
        out
    }

    /// The deterministic, unbounded render (telemetry/tests only; the
    /// production parent path always calls [`ChildHandoff::render_bounded`]).
    pub fn render(&self) -> String {
        self.render_unbounded()
    }

    pub fn approx_tokens(&self) -> usize {
        faktor_context::Estimator.estimate_tokens(&self.render_unbounded())
    }

    /// Deterministically drop whole items until the render fits
    /// `budget_tokens`. Drop order is fixed (decisions, findings, facts,
    /// changed files, then refs — always keeping the session ref), so two
    /// runs of the same handoff produce byte-identical renders. Returns the
    /// number of dropped items.
    pub fn truncate_to_tokens(&mut self, budget_tokens: usize) -> usize {
        let mut dropped = 0usize;
        loop {
            if self.approx_tokens() <= budget_tokens {
                return dropped;
            }
            if self.decisions.pop().is_some()
                || self.findings.pop().is_some()
                || self.facts.pop().is_some()
                || self.changed_files.pop().is_some()
            {
                dropped += 1;
                continue;
            }
            if self.refs.len() > 1 {
                self.refs.pop();
                dropped += 1;
                continue;
            }
            if !self.outcome.is_empty() {
                self.outcome.clear();
                dropped += 1;
                continue;
            }
            if !self.goal.is_empty() {
                self.goal.clear();
                dropped += 1;
                continue;
            }
            // Fixed envelope only: nothing left to drop.
            return dropped;
        }
    }

    /// The bounded production render: the parent-visible handoff text never
    /// exceeds `budget_tokens` (a `0` budget renders nothing at all).
    pub fn render_bounded(&self, budget_tokens: usize) -> String {
        if budget_tokens == 0 {
            return String::new();
        }
        let mut copy = self.clone();
        copy.truncate_to_tokens(budget_tokens);
        copy.render_unbounded()
    }
}

pub(crate) const INDEX_EVIDENCE_MAX_PROMPT_BYTES: usize = 512 * 1024;

pub(crate) const INDEX_EVIDENCE_CONCEPT_MAX: usize = 16;

pub(crate) const INDEX_EVIDENCE_CONCEPT_MIN_CHARS: usize = 4;

/// Bound of durable learning-corpus evidence items emitted per turn (audits
/// 65-69/82): at most this many `learning:<digest>` DATA items, so a large
/// mined corpus can never flood the context even when every entry matches.
pub(crate) const LEARNING_EVIDENCE_MAX: usize = 8;

/// Bound of recent durable FAILED verification records inspected to build
/// the turn's current failure-fingerprint set.
pub(crate) const LEARNING_FAILURE_SCAN: usize = 8;

/// The `[evidence:data]` envelope of learning evidence: the frozen marker
/// strings of `faktor_evidence::provenance` (the agent does not depend on
/// the evidence crate directly; learning metadata is DATA by construction,
/// never instructions).
pub(crate) const LEARNING_EVIDENCE_DATA_MARKER: &str = "[evidence:data]";

pub(crate) const LEARNING_EVIDENCE_DATA_END: &str = "[/evidence:data]";

/// Stall watchdog poll cadence inside a provider stream (bounded tick;
/// mirrors the guarded transports' cancellation checks).
pub(crate) const STALL_POLL_MS: u64 = 250;

/// Ephemeral-stream flush cadence: durable parts are written in segments of
/// this size (plus the final tail), so per-token journaling never happens.
pub(crate) const STREAM_FLUSH_BYTES: usize = 8 * 1024;

/// Hard wall budget of the legacy evidence degrade (audit 14/26): when the
/// IndexService cannot be hosted, the old bounded-scan provider is awaited
/// under this cap, so a slow or stuck provider can never hold a turn past
/// it (its future is dropped and the package degrades to empty).
pub(crate) const LEGACY_EVIDENCE_MAX_WAIT: Duration = Duration::from_millis(2000);

// ------------------------------------------------- durable evidence-poll status
//
// The advisory evidence poll used to hand the turn only its (possibly
// empty) package: a provider error/panic/timeout and an honest "nothing
// matched" answer were the SAME durable fact. The runtime now calls the
// typed poll ([`crate::poll_evidence_with_wall_budget_outcome`]) and records
// the typed status in the turn record, bounded, so "retrieval failed" can
// never be mistaken for "no evidence" after a restart.

/// Schema version of the durable evidence-poll encoding. Decoders refuse an
/// unknown version instead of guessing a future shape.
pub(crate) const EVIDENCE_POLL_DURABLE_SCHEMA: u32 = 1;

/// Hard byte bound of ONE durable evidence-poll encoding. The poll itself
/// already bounds its provider message (lib.rs truncates the adversarial
/// 64 KiB input to its own 512-byte diagnostic bound); this is the second,
/// independent ceiling on everything that lands in the turn record, so a
/// future caller or a hostile provider CODE string can never grow the
/// archived row past it.
pub(crate) const EVIDENCE_POLL_DURABLE_MAX_BYTES: usize = 8 * 1024;

/// Per-field cap of the provider message inside the durable encoding
/// (mirrors the lib.rs in-memory bound).
pub(crate) const EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES: usize = 512;

/// Per-field cap of the provider code inside the durable encoding. Codes
/// come from the untrusted provider (`ErrorKind::Provider { code, .. }`)
/// and are NOT bounded by the poll, so the durable record truncates them
/// here just like messages.
pub(crate) const EVIDENCE_POLL_DURABLE_CODE_MAX_BYTES: usize = 512;

/// Truncate one durable field to `max` bytes on a char boundary.
pub(crate) fn bounded_durable_field(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

/// Bounded extra passes of the queue runner after the durable queue-head
/// RE-CHECK failed with a store error. The error is never read as "empty":
/// the gate stays armed and re-checks (with [`QUEUE_HEAD_READ_RETRY_DELAY`]
/// between attempts) until this bound, then the failure is recorded durably
/// and loudly and the gate releases so a broken store cannot wedge the
/// daemon. The durable rows stay pending for the next kick/recovery.
pub(crate) const MAX_QUEUE_HEAD_READ_RETRIES: u32 = 8;

/// Backoff between queue-head re-check attempts after a store read error.
pub(crate) const QUEUE_HEAD_READ_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The configured turn budget as a wall deadline, with the given fallback
/// when the operator opted out of the wall-clock cap (`turn_budget_ms == 0`).
pub(crate) fn bounded_turn_wait(
    handle: &faktor_session::SessionHandle,
    fallback: Duration,
) -> Duration {
    let budget = handle.turn_budget_ms();
    if budget == 0 {
        fallback
    } else {
        Duration::from_millis(budget)
    }
}

// ------------------------------------------------- semantic-provider consult
// (audits 48-54/58/77/79/118/119). The registry is OPTIONAL acceleration:
// only a REGISTERED provider (never the generic fallback) is consulted,
// every consult is bounded and guarded (a crash, panic, park or error
// degrades to conservative Unknown risk), provider payloads are rendered as
// provenance-tagged DATA, and provider data can only ever NARROW decisions
// (capabilities, tool parallelism, quality floors) — never widen them.

/// Hard wall budget of one semantic consult (mirrors the legacy evidence
/// degrade): a slow or parked provider can never hold a turn past it.
pub(crate) const SEMANTIC_CONSULT_MAX_WAIT: Duration = Duration::from_millis(2000);

/// Byte bound of one rendered semantic evidence block (DATA).
pub(crate) const SEMANTIC_EVIDENCE_MAX_CHARS: usize = 4 * 1024;

/// The review-phase quality floor a High/Unknown semantic risk escalates to
/// (audits 54/118/119); the ordinary review floor is 60.
pub const REVIEW_ESCALATED_QUALITY_FLOOR: u8 = 80;

/// Bounded stable entity id for one workspace-relative path: the path itself
/// when it fits the contract, a hashed token otherwise. Invalid paths (the
/// semantic grammar refuses absolute/traversal values) yield None.
pub(crate) fn semantic_entity_id_for(path: &str) -> Option<SemanticEntityId> {
    if path.len() <= MAX_ENTITY_ID_BYTES {
        return SemanticEntityId::parse(path).ok();
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(path.as_bytes());
    let hex = hasher.finalize().to_hex();
    SemanticEntityId::parse(&format!("path:{}", &hex[..32])).ok()
}

/// Live chunk channel capacity in events (audit 41): with a slow consumer,
/// at most this many whole frames can sit in the channel — memory is
/// structurally bounded before the coalescer ever engages.
pub const CHUNK_CHANNEL_CAPACITY: usize = 1024;

/// Byte cap of the sink-side coalescing buffer: pending text deltas beyond
/// this cap drop their OLDEST bytes (the newest always win).
pub const CHUNK_COALESCE_CAP_BYTES: usize = 64 * 1024;

/// Drop the OLDEST bytes of `buf` beyond `cap` (the newest `cap` bytes
/// survive), landing on a UTF-8 char boundary. Returns dropped bytes.
pub(crate) fn front_trim(buf: &mut String, cap: usize) -> usize {
    let over = buf.len().saturating_sub(cap);
    if over == 0 {
        return 0;
    }
    let mut cut = over;
    while cut < buf.len() && !buf.is_char_boundary(cut) {
        cut += 1;
    }
    buf.drain(..cut);
    cut
}

/// The durable recorded-row input total of one canonical usage frame (audit
/// Phase-1 item C): adapters split cache reads/writes off the uncached
/// input counter at their boundary, so every input-category token
/// (uncached + cache reads + cache writes) folds into the row's single
/// input column. Provider-call rows do not persist the cache split — the
/// crash-reconcile settle prices those rows at the input line — and the
/// fold is exact for wires whose cache lines were never a subset of the
/// uncached counter (anthropic-style `input_tokens` excludes cache reads).
pub(crate) fn recorded_input_total(usage: &CanonicalUsage) -> u64 {
    usage
        .uncached_input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens)
}

/// The authoritative settlement override of one reported cost (audit
/// Phase-1 item C): ONLY USD-compatible provider-reported values may
/// override the route-snapshot estimation — the budget treats the passed
/// value as authoritative, so any other currency is refused here. Returns
/// (micro_usd, true) when the cost overrides, (None, false) when refused.
pub(crate) fn authoritative_reported_micro(cost: &ReportedCost) -> Option<u64> {
    if cost.is_usd() {
        Some(cost.micro_usd)
    } else {
        None
    }
}

/// The durable reservation link of an attempt-keyed provider-call row: the
/// unbudgeted test-ledger marker (0 = [`faktor_session::ReservationId::NOOP`],
/// no durable row) maps to `None` — a fabricated link to nothing must never
/// be persisted; every real reservation (ids start at 1) is preserved.
pub(crate) fn reservation_link(
    reservation: faktor_session::ReservationId,
) -> Option<faktor_session::ReservationId> {
    (reservation.raw() != 0).then_some(reservation)
}

pub struct AgentDeps {
    pub session: Arc<SessionManager>,
    /// Optional live-chunk sink (see [`ChunkEvent`]): bounded + coalescing
    /// under backpressure, never blocks the turn (see [`ChunkSink`]).
    pub chunk_sink: Option<Arc<ChunkSink>>,
    pub providers: Arc<ProviderRegistry>,
    pub permission_requester: Arc<dyn PermissionRequester>,
    pub evidence: Arc<dyn EvidenceProvider>,
    pub tools: Arc<ToolRegistry>,
    /// Content store for tool artifacts (optional).
    pub cas: Option<Arc<faktor_cas::Cas>>,
    /// Workspace registry the runtime opens session workspaces through.
    pub workspaces: Arc<faktor_fs::WorkspaceFileService>,
    /// Transactional edit engine for write_file (None → tool errors).
    pub edit: Option<Arc<faktor_edit::EditEngine>>,
    /// CAS-backed checkpoint store for write_file undo history.
    pub snapshots: Option<Arc<faktor_snapshot::CheckpointStore>>,
    /// Capability policy engine; the runtime roots it at each session's
    /// workspace before handing it to tools.
    pub sandbox: Option<Arc<faktor_sandbox::PermissionEngine>>,
    /// Process supervisor for run_command (None → tool errors).
    pub supervisor: Option<Arc<faktor_terminal::ProcessSupervisor>>,
    /// Verification engine (audit: verification must not depend on the
    /// model's discretion). NON-optional since the typed-verifier migration
    /// (P0-9/10): the runtime derives and executes the checks this turn's
    /// OWN file changes require at every genuine turn end through this
    /// service. "No objective mechanism configured" is the explicit
    /// [`crate::VerificationService::disabled`] state (the old
    /// `verifier: None`), which classifies mutating turns Unverified.
    pub verification: Arc<crate::VerificationService>,
    pub model: String,
    /// Separate compaction model (spec §36); None → deterministic pruning.
    pub compaction_model: Option<String>,
    /// Effective-usage fraction that triggers proactive compaction (0.65–0.70).
    pub compact_at_usage: f64,
    /// Static system instructions (cacheable prefix).
    pub instructions: String,
    /// Lifecycle hooks (audit): deterministic external-process hooks.
    pub hooks: Option<Arc<faktor_hooks::HookRegistry>>,
    /// Per-workspace lazy rule/skill instructions (P0-32): resolve is
    /// consulted when the workspace repo knowledge is loaded. Resolution
    /// goes through the session's DURABLE workspace root — never the
    /// process CWD and never a static config default root; sessions whose
    /// workspace carries no root resolve to
    /// `faktor_instructions::LoadedInstructions::Empty` (documented, not an
    /// error). Hostile trees (oversized authority rule files) resolve to a
    /// typed error that the runtime surfaces, never a silent truncation.
    pub instructions_resolver: Arc<faktor_instructions::InstructionResolver>,
    /// Economic routing policy (P0-2/85/87/88): EVERY paid model call is
    /// routed through this policy first — the former "auto" sentinel path
    /// is gone and the session model is never reached without a policy
    /// consult. The policy's decision provider/model override the
    /// session-configured defaults; a decision with an EMPTY provider and
    /// model is the documented passthrough (the session defaults win).
    /// Failures fail closed: only
    /// [`crate::RouteFailure::RouterUnavailable`] may fall back to the
    /// session's configured model (documented + warned); every other
    /// failure is a typed terminal error on the turn.
    pub routing: Arc<dyn crate::RoutingPolicy>,
    /// The durable monetary budget authority (P0-6/12): one reservation
    /// per paid model call, settled/refunded exactly once against the
    /// task's durable cost ledger (`faktor-session::budget`). Replaces the
    /// former in-memory `budget_micro` pseudo-budget; tests that never set
    /// a cap inject `faktor_session::NoopBudget`.
    pub budgets: Arc<dyn faktor_session::BudgetAuthority>,
    pub clock: Arc<dyn Clock>,
    /// Tool-call parsing mode per provider family (local models default to
    /// NativeWithRepair; native typed providers to Native).
    pub tool_call_mode: ToolCallMode,
    /// State-aware provider retry policy (spec §13): a request that failed
    /// before ANY content became durable may retry (network class); once a
    /// tool ran or parts were flushed, never.
    pub retry_policy: faktor_core::retry::RetryPolicy,
    /// Per-tool-call deadline in ms.
    pub tool_deadline_ms: u64,
    /// The OPTIONAL semantic-provider registry (audit 48-54/58/79/118/119):
    /// capability-selected provider DATA (evidence, deltas, affected sets)
    /// that may only ever NARROW decisions. Additive: with only the generic
    /// fallback registered the runtime's semantic consults are skipped
    /// entirely, so every decision stays byte-identical to a provider-less
    /// runtime; a registered provider's failures/panics degrade to
    /// conservative Unknown risk and never fail the turn.
    pub semantic: Arc<faktor_semantic::SemanticProviderRegistry>,
    /// The OPTIONAL failure-aware context prior (audit 68; the learning
    /// crate's `context_prior`/`FailurePrior` contract): a handle that
    /// states, per non-Required candidate, the omission risk the planner
    /// folds into selection (`base * clamp(risk, 1, 2)`, so a prior can only
    /// PROTECT a candidate, never demote it and never observe Required
    /// content). Additive: `None` keeps every plan byte-identical to the
    /// prior-less path. The runtime consults it ONLY when
    /// [`EfficiencyFlags::failure_learning`] is on — the flag AND an
    /// installed handle are both required. Construction sites that never
    /// wire a learning service pass `None`.
    ///
    /// PANIC CONTRACT: the prior is called inline on the planning thread and
    /// a panic is NOT caught (there is no `catch_unwind` on this path) — a
    /// prior must be total, and that is the caller's contract. Hostile
    /// VALUES (NaN/inf/negative/huge) are sanitized by `faktor_context`'s
    /// `prior_adjusted_gain`; the production handle
    /// (`crates/cli/src/main.rs` `LearningRiskPrior`) is additionally total
    /// and clamps every risk to `[1, 2]`.
    pub context_prior: Option<Arc<dyn faktor_context::information::FailurePrior + Send + Sync>>,
    /// The parsed production efficiency flags (all-default-false; see
    /// [`EfficiencyFlags`]). `failure_learning` gates the
    /// [`AgentDeps::context_prior`] seam; the remaining flags are carried
    /// for their efficiency components.
    pub efficiency: EfficiencyFlags,
    /// The daemon's configured-secret registry: the SAME `Arc` the outbound
    /// egress scan uses (built once in the daemon builder). Tool outcomes,
    /// durable message-part excerpts and every CAS artifact put are exact-
    /// redacted through it, so a configured provider key echoed by a tool
    /// can neither reach durable storage nor block the next provider request
    /// at the egress scan. `None` in test graphs (pattern-only behavior).
    pub secret_registry: Option<Arc<faktor_security::registry::SecretRegistry>>,
}

impl AgentDeps {
    pub fn artifact_sink(&self, session: SessionId) -> ToolArtifactSink {
        match &self.cas {
            Some(cas) => ToolArtifactSink::Real {
                writer: Arc::new(ArtifactWriter::new(cas.clone(), session)),
                secrets: self.secret_registry.clone(),
            },
            None => ToolArtifactSink::Null,
        }
    }
}

/// The per-session queue-runner gate (boundary-race authority): at most one
/// runner owns a session's durable queue. Every additional request — a new
/// queued submit, the executor's settle-path kick after a turn
/// finishes/cancels, or a recovery kick — ARMS the live runner for exactly
/// one more BOUNDED pass instead of being dropped. The runner consumes the
/// armed pass under this gate before releasing it, so a request arriving
/// while the previous runner is exiting on its wait-budget boundary (the
/// boundary race) can never leave a durable head without a runner.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct QueueRunnerGate {
    /// An additional bounded pass was requested while this gate was held.
    pub(crate) rerun: bool,
    /// Bounded passes this runner STARTED (diagnostics/tests: `>= 2` proves
    /// an armed pass was consumed instead of being lost).
    pub(crate) passes: u64,
    /// Consecutive durable queue-head READ failures of this gate. A store
    /// error is never treated as an empty queue: the gate stays armed while
    /// this counter is under [`MAX_QUEUE_HEAD_READ_RETRIES`], then the runner
    /// records a durable marker and releases the gate (bounded — a broken
    /// store must not wedge the daemon). The durable rows stay pending for
    /// the next kick/recovery.
    pub(crate) pending_read_failures: u32,
}

pub struct AgentRuntime {
    pub(crate) deps: Arc<AgentDeps>,
    /// Sessions with a live queue-runner task (single runner per session);
    /// the value is the gate arming state (see [`QueueRunnerGate`]).
    pub(crate) runners: std::sync::Mutex<std::collections::HashMap<SessionId, QueueRunnerGate>>,
    /// Per-session bounded progress records (stall vs progress, §28):
    /// `{last_output_at, last_progress_at, in_flight_op,
    /// last_op_completed_at}` per live session, fed from op completions,
    /// tool events and output chunks.
    pub(crate) progress: std::sync::Mutex<std::collections::HashMap<SessionId, StallTracker>>,
    /// Per-child coordination-notice memo, keyed by child session and folded
    /// by the run-family BOARD REVISION (P2): the unread scan runs once per
    /// revision, and intermediate board changes coalesce into the single
    /// "coordination: N unread; use board_read" line — automatic prompt
    /// growth is O(1) in the board size, and a body can never ride it.
    pub(crate) coordination:
        std::sync::Mutex<std::collections::HashMap<SessionId, CoordinationNoticeMemo>>,
    /// The runtime's token-count cache (P0-81): bounded LRU keyed by
    /// (model tokenizer identity, content hash), shared by every session
    /// this runtime plans for. Interior mutex: wire planning holds it
    /// briefly for lookups/inserts only — misses estimate outside the lock.
    pub(crate) token_cache: TokenCache,
    /// Stall-silence budget in ms (see [`DEFAULT_STALL_SILENCE_MS`]); 0
    /// disables time-stall verdicts.
    pub(crate) stall_silence_ms: std::sync::atomic::AtomicU64,
    /// Verification/review quality override (audit 92): 0 = unset —
    /// mutating turns run Strict, non-mutating turns run Normal (see
    /// [`VerificationQuality`]); 1 = Normal forced; 2 = Strict forced.
    pub(crate) quality_mode: std::sync::atomic::AtomicU8,
    /// The durable repository IndexService (audits 30/64), created lazily
    /// from the session store + workspace registry on the first turn that
    /// resolves a workspace. When a workspace has a Ready generation the
    /// per-turn evidence is served from the index; while it is Building the
    /// bounded evidence scan stays the fallback — a first prompt NEVER
    /// blocks on a full index build. `None` when the service could not be
    /// hosted (no store/fs); the bounded scan is then always used.
    pub(crate) index_service:
        std::sync::OnceLock<Option<std::sync::Arc<faktor_index::IndexService>>>,
    /// THE durable evidence authority (schema v21): the ONE evidence store
    /// of the runtime. Producers — the index/cold/legacy retrieval ladder,
    /// the semantic registry, the learning corpus, tool outputs and large
    /// reads — archive normalized/compressed evidence here; the
    /// [`ContextCompiler`] selects from it. Backing bytes live in
    /// `<store root>/evidence-cas`, so a daemon restart reopens the same
    /// ids and the same retrievable backing.
    pub(crate) evidence_authority: Arc<DurableEvidenceAuthority>,
    /// The additive commercial debit authority (Wave 5 residual): `None`
    /// (the default, and the billing-disabled daemon) keeps every dispatch
    /// byte-identical to the pre-billing runtime. Installed by the host
    /// after construction through [`AgentRuntime::set_provider_debits`].
    pub(crate) provider_debits:
        std::sync::Mutex<Option<Arc<dyn crate::credits::ProviderAttemptDebits>>>,
    /// Additive provider-call retry-policy override (test/tuning seam).
    /// `None` (always, on every production path) keeps
    /// `AgentDeps::retry_policy` verbatim. The production-wiring
    /// certification installs a multi-attempt server-class policy through
    /// [`AgentRuntime::set_retry_policy`] to prove a transient provider
    /// error retries the SAME planned request (media parts included) without
    /// duplication.
    pub(crate) retry_policy_override: std::sync::Mutex<Option<faktor_core::retry::RetryPolicy>>,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub op_id: OpId,
    pub final_state: AgentState,
    pub turns: u32,
    pub compacted: bool,
    pub loop_stopped: bool,
    /// True when the turn stopped because several consecutive model
    /// iterations produced no new durable state (stall detection).
    pub stalled: bool,
    /// True when the prompt was durably QUEUED (another turn was active):
    /// the per-session turn runner delivers it later. No work was started.
    pub queued: bool,
    /// End-of-turn verification results `(check id, passed)` for the checks
    /// this turn's file changes required. Empty when no verifier is wired,
    /// no files changed, or the workspace did not resolve.
    pub verification: Vec<(String, bool)>,
    /// End-of-turn acceptance over the REQUIRED checks; None when no
    /// verification ran (infra absence or nothing to verify).
    pub acceptance: Option<faktor_verify::Acceptance>,
    /// Independent completion-review evidence (audit round 14: completion
    /// skepticism): a structured {suspects, blocking, verdict, evidence}
    /// value computed from the changed files' bounded heads at the same
    /// genuine turn ends and under the same verifier/workspace conditions as
    /// [`TurnOutcome::verification`]. None when no verifier is wired, no
    /// files changed, or the workspace did not resolve. The review is
    /// advisory: it never fails the turn, but a blocking verdict downgrades
    /// the completion gate to [`CompletionGate::BlockedVerification`].
    pub review: Option<serde_json::Value>,
    /// Completion gate (audits 4/6/7): the durable classification computed
    /// at the genuine end-of-turn verification site. None when the turn
    /// changed no files (nothing to gate) or the turn did not reach a
    /// genuine end. Some(Unverified) means the change is NOT claimable as
    /// complete — no objective mechanism ran. Some(FailedVerification) /
    /// Some(BlockedVerification) mean the change is not verified complete
    /// (reasons name the failed/unavailable checks, the review finding, the
    /// budget refusal or the criteria divergence, each with a machine
    /// [`ReasonCode`]). Only Some(VerifiedComplete) presents the turn's
    /// work as complete.
    pub completion: Option<CompletionGate>,
    /// Machine reason for a turn that stopped without a genuine completion
    /// (audit 94): stall (`stalled`), loop stop (`loop_stopped`), hard
    /// budget denial, or cancellation. None for genuine ends, queued turns
    /// and turns that failed for other reasons (the journal message stays
    /// the human record there). Codes and details match the prose the
    /// failing paths journaled.
    pub stop_reason: Option<OutcomeReason>,
    /// Conservative semantic-provider risk of this turn (audits 54/118/119):
    /// `Some(level)` only when a REGISTERED provider answered the turn's
    /// semantic consult. `High`/`Unknown` escalate reviewer strength,
    /// verification quality and reduce tool/child parallelism — Unknown is
    /// never silently safe. `None` = no provider was consulted: every
    /// risk-driven decision keeps today's behavior exactly (parity).
    pub semantic_risk: Option<faktor_semantic::RiskLevel>,
    /// Bounded, machine-readable diagnostics of the turn's advisory
    /// evidence poll ([`crate::EvidencePollStatus`] encoded by
    /// [`encode_evidence_poll_status`], the SAME canonical bytes archived
    /// durably under `evidence-poll:<turn_op>`). `None` when the legacy
    /// provider was never polled (the index/cold ladder served). Some
    /// status makes "retrieval failed / panicked / timed out" at the turn
    /// boundary distinguish-able from an honest "no evidence" answer; the
    /// advisory policy still lets the turn proceed either way (a degraded
    /// poll never fails the turn by itself).
    pub evidence_poll: Option<String>,
}

/// Random-ish uniqueness tag of one marker name: pid + a `RandomState`-keyed
/// hash of the clock and sequence. The name must never rely on the clock
/// alone — two processes sharing one store root can observe the same
/// millisecond.
pub(crate) fn marker_random_tag(at_ms: i64, seq: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_i32(std::process::id() as i32);
    hasher.write_i64(at_ms);
    hasher.write_u64(seq);
    hasher.finish()
}

/// Bound the marker directory: when it holds more than
/// [`durable_write_marker_dir_max`] files, consume the OLDEST terminal
/// (non-pending, unreadable or corrupt) markers, loudly, until the bound is
/// met. Pending markers are NEVER deleted here — if only pending work
/// remains over the bound, that is a loud error, not a silent loss.
pub(crate) fn gc_durable_write_markers(dir: &std::path::Path) {
    let bound = durable_write_marker_dir_max();
    let entries: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => return,
    };
    if entries.len() <= bound {
        return;
    }
    let mut rows: Vec<(std::time::SystemTime, std::path::PathBuf, bool)> = entries
        .into_iter()
        .map(|path| {
            let mtime = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let pending = std::fs::read(&path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
                .and_then(|marker| {
                    marker
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(|status| status == "pending")
                })
                .unwrap_or(false);
            (mtime, path, pending)
        })
        .collect();
    rows.sort_by_key(|(mtime, _, _)| *mtime);
    let mut remaining = rows.len();
    for (_, path, pending) in rows {
        if remaining <= bound {
            break;
        }
        if pending {
            continue; // durable work is never GC'd
        }
        tracing::warn!(
            path = %path.display(),
            bound,
            "durable-write marker directory is over its bound; consuming the oldest terminal marker"
        );
        if std::fs::remove_file(&path).is_ok() {
            remaining -= 1;
        }
    }
    if remaining > bound {
        tracing::error!(
            remaining,
            bound,
            "durable-write marker directory is still over its bound; only pending markers remain \
             (they are never deleted — the flood is surfaced, not silently dropped)"
        );
    }
}

/// Bounded read of one marker file (the bound is adversarial: a huge file is
/// refused, never slurped).
pub(crate) fn read_bounded_file(
    path: &std::path::Path,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)?;
    let mut raw = Vec::new();
    file.take(max_bytes as u64 + 1).read_to_end(&mut raw)?;
    if raw.len() > max_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "marker exceeds the bound",
        ));
    }
    Ok(raw)
}

pub(crate) fn remove_marker_file(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        tracing::error!(path = %path.display(), "durable-write marker could not be removed: {e}");
    }
}

pub(crate) fn state_tag(s: AgentState) -> String {
    serde_json::to_string(&s)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

pub(crate) fn effect_tag(e: EffectStatus) -> &'static str {
    match e {
        EffectStatus::Unknown => "unknown",
        EffectStatus::Verified => "verified",
        EffectStatus::Applied => "applied",
        EffectStatus::Failed => "failed",
    }
}

/// Redact any credential the text echoes (default SecretPolicy) and bound
/// the result; benign text stays byte-identical.
pub(crate) fn redact_secrets_bounded(text: &str, max: usize) -> String {
    let policy = faktor_security::SecretPolicy::default();
    let bounded = truncate(text, max);
    if faktor_security::scan_secrets(&bounded, &policy).is_empty() {
        bounded
    } else {
        truncate(&faktor_security::redact(&bounded, &policy), max)
    }
}

/// The durable excerpt of a call the crash left unresolved (see
/// [`AgentRuntime::answer_dangling_tool_calls`]): typed and honest, never a
/// fabricated tool outcome.
pub(crate) fn interrupted_tool_excerpt(name: &str) -> String {
    truncate(
        &format!(
            "tool call unresolved (interrupted): the turn was interrupted before {name} was \
             resolved; no outcome was recorded"
        ),
        2000,
    )
}

/// Read a required string field from a durable part payload; a missing or
/// non-string field is loud corruption, never silently dropped.
pub(crate) fn str_field(data: &serde_json::Value, key: &str) -> faktor_core::Result<String> {
    data.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| Error::malformed(format!("durable part is missing string field `{key}`")))
}

/// Cheap deterministic fold (blake3, length-prefixed parts) to u64 — the
/// same shape the stall tracker's semantic hashes use (P0-78: cheap
/// hashes, never an LLM).
pub(crate) fn evidence_fold_hash(parts: &[&[u8]]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    for p in parts {
        hasher.update(&(p.len() as u64).to_le_bytes());
        hasher.update(p);
    }
    let out = hasher.finalize();
    u64::from_le_bytes(out.as_bytes()[..8].try_into().expect("8 bytes"))
}

/// Deterministic bounded token claim of the loaded recent turns (same
/// estimator contract as [`estimate_evidence_tokens`]).
pub(crate) fn estimate_recent_tokens(recent: &[RecentTurn]) -> u32 {
    recent.iter().fold(0u32, |acc, turn| {
        acc.saturating_add(u32::try_from(turn.text.len() / 3 + 1).unwrap_or(u32::MAX))
    })
}

/// Failure fingerprint of one failed verification check (P0-78/79): hash
/// over `(check id, summary)`. Same id + same summary = the same failure
/// state; a different summary = the task moved through the failure space.
pub(crate) fn failure_fingerprint_of(check_id: &str, summary: Option<&str>) -> u64 {
    let empty = "";
    let refs = [check_id.as_bytes(), summary.unwrap_or(empty).as_bytes()];
    evidence_fold_hash(&refs)
}

/// Honest test-command detection (prefixes only — "test" alone matches
/// "latest"/"attest").
pub(crate) fn looks_like_test_command(cmd: &str) -> bool {
    let c = cmd.trim_start();
    c.starts_with("cargo test")
        || c.starts_with("cargo nextest")
        || c.starts_with("pytest")
        || c.starts_with("python -m pytest")
        || c.starts_with("npm test")
        || c.starts_with("npm run test")
        || c.starts_with("yarn test")
        || c.starts_with("go test")
        || c.starts_with("pnpm test")
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

// ------------------------------------------------------------- completion
// review (audit round 14: independent completion skepticism)
//
// review_signals + review_verdict are PURE (no provider, no I/O) so the
// review pass is deliberately separate from the implementing context: cheap
// head-only heuristics that a cheap reviewer model (or a daemon rule) can
// turn into "did we actually do the work" evidence. Every scan is bounded to
// the first 400 chars of the content heads the caller hands in; callers cap
// reads at 400 bytes per file.

/// Head of a changed file that a placeholder/TODO marker would hide in.
pub(crate) const REVIEW_HEAD_CHARS: usize = 400;

/// Case-insensitive completion-placeholder tokens scanned per head.
pub(crate) const REVIEW_TODO_TOKENS: &[&str] = &["todo", "fixme", "hack", "xxx"];

/// Test-marker fragments scanned per head (kept case-sensitive: these are
/// language literals, not prose).
pub(crate) const REVIEW_TEST_MARKERS: &[&str] = &["#[test]", "describe(", "it(", "def test_"];

/// Assertion tokens: a head with test markers but none of these is a
/// weakened-test suspect.
pub(crate) const REVIEW_ASSERTION_TOKENS: &[&str] = &["assert", "expect", "should", "equal"];

/// Head lines equal to a stub body (trimmed) count as placeholders.
pub(crate) const REVIEW_STUB_LINES: &[&str] = &["...", "// todo: implement"];

/// Criteria stopwords: tokens that carry no topical signal about what files
/// should have changed.
pub(crate) const REVIEW_STOPWORDS: &[&str] = &[
    "a", "about", "again", "all", "an", "and", "any", "are", "as", "at", "be", "been", "being",
    "between", "both", "but", "by", "can", "could", "did", "do", "does", "each", "few", "for",
    "from", "had", "has", "have", "he", "her", "here", "how", "i", "if", "in", "into", "is", "it",
    "its", "just", "may", "me", "more", "most", "must", "my", "no", "not", "now", "of", "off",
    "on", "only", "or", "other", "our", "out", "over", "own", "same", "she", "should", "so",
    "some", "such", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "those", "to", "too", "under", "up", "us", "very", "was", "we", "were", "what", "when",
    "where", "which", "while", "who", "why", "will", "with", "would", "you", "your",
];

/// Structured per-change completion evidence (see review_signals doc):
/// every field is derived from the bounded head; nothing here reads files.
pub(crate) fn review_signals(
    changed_files: &[String],
    repo_snapshot: &[(String, String)],
) -> serde_json::Value {
    let snap: HashMap<&str, &str> = repo_snapshot
        .iter()
        .map(|(p, h)| (p.as_str(), h.as_str()))
        .collect();
    let mut files: Vec<serde_json::Value> = Vec::new();
    let mut todo_files: Vec<String> = Vec::new();
    let mut placeholder_files: Vec<String> = Vec::new();
    let mut weakened_test_files: Vec<String> = Vec::new();
    for path in changed_files.iter() {
        let mut contains_todo = false;
        let mut placeholder_detected = false;
        let mut head_test_marker = false;
        let mut assertion_seen = false;
        let mut head_chars = 0usize;
        let mut unread = true;
        if let Some(raw) = snap.get(path.as_str()) {
            // Bounded by construction: the fn itself never looks past the
            // first 400 chars of whatever head it is handed (hostile-head
            // callers cannot widen the scan).
            let head: String = raw.chars().take(REVIEW_HEAD_CHARS).collect();
            head_chars = head.chars().count();
            let lower = head.to_lowercase();
            contains_todo = REVIEW_TODO_TOKENS.iter().any(|t| lower.contains(t));
            head_test_marker = REVIEW_TEST_MARKERS.iter().any(|m| head.contains(m));
            assertion_seen = REVIEW_ASSERTION_TOKENS.iter().any(|t| lower.contains(t));
            placeholder_detected = review_placeholder(&head);
            unread = false;
        }
        // Test-likeness rides the path AND the head: a file we could not
        // read still contributes its path signal.
        let looks_like_test = review_path_is_test(path) || head_test_marker;
        // Weakened test: head has test markers but zero assertion tokens.
        let weakened = head_test_marker && !assertion_seen;
        let entry = serde_json::json!({
            "path": path,
            "head_chars": head_chars,
            "unread": unread,
            "contains_todo": contains_todo,
            "looks_like_test": looks_like_test,
            "placeholder_detected": placeholder_detected,
            "weakened_test_suspect": weakened,
        });
        if contains_todo {
            todo_files.push(path.clone());
        }
        if placeholder_detected {
            placeholder_files.push(path.clone());
        }
        if weakened {
            weakened_test_files.push(path.clone());
        }
        files.push(entry);
    }
    serde_json::json!({
        "files": files,
        "todo_files": todo_files,
        "placeholder_files": placeholder_files,
        "weakened_test_files": weakened_test_files,
    })
}

/// True when the bounded head looks like an unfinished body: a stub line
/// ("...", "// todo: implement") or fewer than 60 chars of actual code
/// (comment-only heads and tiny scaffolds count — they are placeholders).
pub(crate) fn review_placeholder(head: &str) -> bool {
    if head.lines().any(|l| {
        let t = l.trim().to_lowercase();
        REVIEW_STUB_LINES.contains(&t.as_str())
    }) {
        return true;
    }
    review_code_chars(head) < 60
}

/// Non-comment code characters in the head (whitespace dropped, inline "//"
/// comments cut, comment-only lines skipped). "#[attr]" and "#!shebang"
/// lines are code, "# comment" lines are not.
pub(crate) fn review_code_chars(head: &str) -> usize {
    let mut n = 0usize;
    for line in head.lines() {
        let t = line.trim_start();
        if t.is_empty()
            || t.starts_with("//")
            || t.starts_with("/*")
            || t.starts_with('*')
            || t.starts_with("#!")
            || t == "#"
            || t.starts_with("# ")
        {
            continue;
        }
        let code = t.split("//").next().unwrap_or("");
        n += code.chars().filter(|c| !c.is_whitespace()).count();
    }
    n
}

/// Verdict over the evidence: warn-level suspects (never fatal) plus
/// blocking reasons (weakened tests; placeholder/TODO inside changed code).
/// The runtime now seeds the once-only durable criteria row (goal + derived
/// checks) at genuine turn ends, but the review's relevance check still runs
/// with an empty criteria list here — when non-empty (unit/downstream
/// callers) a crude token relevance check adds a warn suspect when no
/// changed file name shares any non-stopword token with any criterion.
pub(crate) fn review_verdict(
    evidence: &serde_json::Value,
    criteria: &[String],
) -> serde_json::Value {
    let mut suspects: Vec<String> = Vec::new();
    let mut blocking: Vec<String> = Vec::new();
    let mut changed_paths: Vec<String> = Vec::new();
    if let Some(files) = evidence.get("files").and_then(|v| v.as_array()) {
        for f in files {
            let path = f.get("path").and_then(|v| v.as_str()).unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            changed_paths.push(path.to_string());
            let todo = f
                .get("contains_todo")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let placeholder = f
                .get("placeholder_detected")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let weakened = f
                .get("weakened_test_suspect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if weakened {
                blocking.push(format!("weakened test file without assertions: {path}"));
            }
            if todo && placeholder {
                blocking.push(format!("placeholder/TODO in changed code: {path}"));
            } else if todo {
                suspects.push(format!("contains TODO in changed file: {path}"));
            } else if placeholder && !weakened {
                suspects.push(format!("placeholder body in changed file: {path}"));
            }
        }
    }
    let criteria_reviewed = !criteria.is_empty();
    if criteria_reviewed {
        let criterion_tokens: Vec<String> = criteria
            .iter()
            .flat_map(|c| review_tokens(c))
            .filter(|t| !REVIEW_STOPWORDS.contains(&t.as_str()))
            .collect();
        let addressed = changed_paths.iter().any(|p| {
            review_tokens(p)
                .iter()
                .any(|t| criterion_tokens.contains(t))
        });
        if !addressed {
            suspects.push("criteria not obviously addressed by changed files".to_string());
        }
    }
    serde_json::json!({
        "suspects": suspects,
        "blocking": blocking,
        "verdict": if blocking.is_empty() { "pass" } else { "block" },
        "criteria_reviewed": criteria_reviewed,
    })
}

/// Lowercased alphanumeric word tokens (single chars dropped).
pub(crate) fn review_tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() > 1)
        .collect()
}

/// Path-based test-likeness: any path component token is one of
/// test/tests/spec/specs (word-boundaried — "contest" never matches).
pub(crate) fn review_path_is_test(path: &str) -> bool {
    path.split(|c: char| !c.is_alphanumeric())
        .map(|s| s.to_lowercase())
        .any(|s| matches!(s.as_str(), "test" | "tests" | "spec" | "specs"))
}

/// End-of-turn completion review for one logical turn (audit round 14):
/// read each changed file's head through the workspace handle (bounded to
/// the first 400 bytes, at most 16 files), run the pure signal scan, and
/// derive the verdict against an empty criteria list (the once-only
/// criteria row seeded at genuine ends is not fed back into the relevance
/// check yet — documented at the TurnOutcome field). Unreadable paths
/// are skipped per-file; the whole review is advisory and never errors.
pub(crate) fn collect_review_verdict(
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
) -> Option<serde_json::Value> {
    const HEAD_BYTES: usize = 400;
    const MAX_FILES: usize = 16;
    let mut snapshot: Vec<(String, String)> = Vec::new();
    for p in changed.iter().take(MAX_FILES) {
        // Hard 400-byte cap at the filesystem read (never the whole file,
        // whatever its size).
        let Ok(data) = ws.read(std::path::Path::new(p), HEAD_BYTES) else {
            continue;
        };
        let head = String::from_utf8_lossy(&data.bytes);
        snapshot.push((p.clone(), head.into_owned()));
    }
    let signals = review_signals(changed, &snapshot);
    let mut verdict = review_verdict(&signals, &[]);
    if let Some(obj) = verdict.as_object_mut() {
        obj.insert("evidence".to_string(), signals);
    }
    Some(verdict)
}

// ------------------------------------------------------------- structured
// completion review (audit round 15: P0-12/80 structured diff package over
// the checkpoint/CAS base + P0-13 independent review-model call for risky
// changes). The pure building blocks (statuses, inventory, risk map,
// package bounds/render, typed verdict parse) live in
// `faktor_verify::review`; this region owns the I/O: checkpoint rows, CAS
// blobs, bounded workspace reads, routing, and the isolated review call.

/// The review package size target (mirror of
/// `faktor_verify::review::REVIEW_PACKAGE_MAX_BYTES`; kept in sync — a
/// package beyond it is a hard Oversized refusal, never a partial review).
/// Per-side content bound for one diffed file (mirror of the diff engine's
/// bound; a side beyond it cannot be diffed honestly).
pub(crate) const REVIEW_SIDE_BOUND: usize = faktor_verify::review::REVIEW_SIDE_MAX_BYTES;

/// Added-line chars scanned per file for the structured LOCAL signals
/// (bounded; the full added lines ride the package hunks the model sees).
pub(crate) const REVIEW_ADDED_SCAN_CHARS: usize = 16 * 1024;

/// Bounded review-model output accumulation (a verdict is small; anything
/// beyond this bound fails the call — a partial verdict never parses).
pub(crate) const REVIEW_MODEL_MAX_TEXT_CHARS: usize = 8 * 1024;

/// One review-model call deadline (bounded stream; the request carries a
/// child of the turn's cancellation token so a user Stop aborts it).
pub(crate) const REVIEW_MODEL_CALL_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) const REVIEW_EVIDENCE_MAX_HUNK_ROWS: usize = 24;

pub(crate) const REVIEW_EVIDENCE_MAX_PATH_ENTRIES: usize = 16;

pub(crate) const REVIEW_EVIDENCE_MAX_CRITERIA: usize = 8;

/// Bounded criteria rendered into the no-op reviewer package (the same cap
/// the structured package uses; the no-op proof is a review of the goal,
/// not of an unbounded criterion list).
pub(crate) const REVIEW_NO_OP_MAX_CRITERIA: usize = 8;

/// The independent reviewer's system prompt: a SEPARATE contract from the
/// agent instructions and the transcript. The reviewer receives ONLY the
/// diff package + criteria + this prompt (P0-13 context isolation).
pub(crate) const REVIEW_MODEL_SYSTEM: &str =
    "You are the independent completion reviewer of a software change. \
Your ONLY inputs are the acceptance criteria and the structured diff package below. \
You have NO knowledge of the implementation conversation, the task prompt, or the \
agent's reasoning — judge ONLY the change package. Be skeptical: verify the change \
addresses the criteria, look for placeholder or hollowed code, removed or disabled \
tests, deleted tests without replacement, and content that looks accidental or \
hostile. Respond with a SINGLE JSON object of exactly this shape: \
{\"verdict\": \"clean\"|\"concern\"|\"block\", \"findings\": [\"...\"]} \
where clean = no findings, concern = advisory findings that do not block, \
block = the change must not complete as-is. No prose outside the JSON object.";

/// The durable typed task row's acceptance-criteria entries (the same rows
/// the verification site syncs against; unseeded rows yield empty).
pub(crate) fn review_durable_criteria(handle: &faktor_session::SessionHandle) -> Vec<String> {
    let mut tasks = handle.list_tasks().unwrap_or_default();
    if tasks.is_empty() {
        return Vec::new();
    }
    let preferred = handle.task_id().ok();
    let pos = tasks
        .iter()
        .position(|t| Some(t.task_id) == preferred)
        .or(Some(0));
    pos.and_then(|p| tasks.get_mut(p))
        .map(|t| t.acceptance_criteria.clone())
        .unwrap_or_default()
}

/// The acceptance-criteria entries the review package carries (P0-12):
/// first the once-only derivation the verification site freezes
/// ([`criteria_rows`]), then the durable typed task row, then the goal text
/// — never empty when a goal exists.
pub(crate) fn review_criteria_entries(
    goal: &str,
    checks: &[faktor_verify::Check],
    handle: &faktor_session::SessionHandle,
) -> Vec<String> {
    if let Some(rows) = criteria_rows(goal, checks) {
        if !rows.is_empty() {
            return rows;
        }
    }
    let durable = review_durable_criteria(handle);
    if !durable.is_empty() {
        return durable;
    }
    let goal = goal.trim();
    if goal.is_empty() {
        Vec::new()
    } else {
        vec![format!("goal: {}", truncate(goal, 200))]
    }
}

/// Check-result rows the package carries (the checks are derived but not yet
/// RUN at the review decision — status `not_run` is the honest state).
pub(crate) fn review_check_rows(
    checks: &[faktor_verify::Check],
) -> Vec<faktor_verify::review::CheckResult> {
    let mut rows: Vec<faktor_verify::review::CheckResult> = checks
        .iter()
        .take(faktor_verify::review::REVIEW_MAX_CHECK_RESULTS)
        .map(|c| faktor_verify::review::CheckResult {
            id: truncate(&c.id, 128),
            status: "not_run".into(),
            summary: truncate(&c.command, 160),
        })
        .collect();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows.dedup();
    rows
}

pub(crate) fn file_hash_from_row(hex: &str) -> Option<faktor_core::hash::FileHash> {
    if hex.is_empty() {
        None
    } else {
        faktor_core::hash::FileHash::from_hex(hex)
    }
}

/// Bounded whole-content read: Ok(Some(bytes)) when the file exists and is
/// at most the side bound; Ok(None) when it exists but is too large; Err
/// when missing/unreadable.
pub(crate) fn ws_read_bounded(
    ws: &faktor_fs::WorkspaceHandle,
    path: &str,
) -> Result<Option<Vec<u8>>, ()> {
    match ws.read(std::path::Path::new(path), REVIEW_SIDE_BOUND + 1) {
        Ok(data) => {
            if data.bytes.len() > REVIEW_SIDE_BOUND {
                Ok(None)
            } else {
                Ok(Some(data.bytes))
            }
        }
        Err(_) => Err(()),
    }
}

/// Map one file's before/after bytes through the bounded diff engine into
/// the pure hunk shape. A coarse outcome (input beyond the engine's honest
/// bounds) refuses the review — a mode marker is never a line diff.
pub(crate) fn review_hunks_for(
    path: &str,
    before: &[u8],
    after: &[u8],
) -> Result<Vec<faktor_verify::review::Hunk>, String> {
    if before == after {
        return Ok(Vec::new());
    }
    let outcome = faktor_edit::diff::diff_hunks(before, after);
    if outcome.mode == faktor_edit::diff::DiffMode::Coarse {
        return Err(format!(
            "{path} exceeds the diff engine's honest line-diff bounds"
        ));
    }
    let mut hunks = Vec::new();
    for h in &outcome.hunks {
        if h.lines.len() > faktor_verify::review::REVIEW_MAX_LINES_PER_HUNK {
            return Err(format!("{path} has an oversized hunk"));
        }
        hunks.push(faktor_verify::review::Hunk {
            path: path.to_string(),
            old_start: h.old_start,
            old_count: h.old_count,
            new_start: h.new_start,
            new_count: h.new_count,
            lines: h
                .lines
                .iter()
                .map(|l| match l {
                    faktor_edit::diff::DiffLine::Context(t) => faktor_verify::review::DiffLine {
                        kind: faktor_verify::review::DiffKind::Context,
                        text: t.clone(),
                    },
                    faktor_edit::diff::DiffLine::Removed(t) => faktor_verify::review::DiffLine {
                        kind: faktor_verify::review::DiffKind::Removed,
                        text: t.clone(),
                    },
                    faktor_edit::diff::DiffLine::Added(t) => faktor_verify::review::DiffLine {
                        kind: faktor_verify::review::DiffKind::Added,
                        text: t.clone(),
                    },
                })
                .collect(),
        });
    }
    Ok(hunks)
}

/// Heuristic assertion-token check (bounded; never proof): any word token
/// starting with assert/expect.
pub(crate) fn line_assertion_like(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with("assert") || w.starts_with("expect"))
}

pub(crate) fn truncate_300(s: &str) -> String {
    truncate(s, 300)
}

/// Changed test paths helper for evidence lists.
pub(crate) fn inventory_list(
    statuses: &[faktor_verify::review::FileStatus],
    pred: impl Fn(&faktor_verify::review::FileStatus) -> bool,
) -> Vec<String> {
    let mut out: Vec<String> = statuses
        .iter()
        .filter(|s| pred(s))
        .map(|s| s.path.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Push a string into a JSON string array (bounded, hostile-safe).
pub(crate) fn review_push_str(list: &mut Vec<String>, item: String) {
    if !list.iter().any(|s| s == &item) {
        list.push(item);
    }
}

/// The completion review decision (P0-12/80 + P0-13): legacy bounded head
/// verdict + structured diff evidence + — for risky changes — the separate
/// review-model call. Never fails the turn; the JSON keeps the
/// `{suspects, blocking, verdict, criteria_reviewed, evidence}` shape the
/// gate and the durable record consume.
pub(crate) async fn independent_completion_review(
    deps: &AgentDeps,
    handle: &faktor_session::SessionHandle,
    ws: &faktor_fs::WorkspaceHandle,
    changed: &[String],
    goal: &str,
    repo_files: &[String],
    cancel: &CancellationToken,
) -> Option<serde_json::Value> {
    // 1. Legacy bounded head scan + verdict (unchanged semantics).
    let mut review = collect_review_verdict(ws, changed)?;
    // 2. Derived checks — the SAME multi-component derivation the
    //    verification site executes: every changed file maps to its owning
    //    component by longest root prefix and contributes ALL of its typed
    //    families; the legacy mirror renders the package rows. A typed
    //    derivation refusal (cap/id conflict) leaves the package with no
    //    derived rows; the verification site independently refuses and never
    //    certifies completion.
    let checks: Vec<faktor_verify::Check> = if repo_files.is_empty() {
        Vec::new()
    } else {
        let profile = faktor_verify::derive::detect_project_profile(ws.root(), repo_files);
        let changed_paths: Vec<std::path::PathBuf> =
            changed.iter().map(std::path::PathBuf::from).collect();
        faktor_verify::derive::derive_checks(&profile, &changed_paths)
            .map(|specs| specs.iter().map(legacy_mirror_of_spec).collect())
            .unwrap_or_default()
    };
    let criteria = review_criteria_entries(goal, &checks, handle);
    let evidence = structured_review_evidence(deps, handle, ws, changed, &criteria, &checks);
    // Semantic review consult (audits 54/77/118/119): when a REGISTERED
    // provider covers the operation, its DATA rides the review evidence and
    // a High/Unknown risk escalates the reviewer's quality floor and forces
    // the separate review-model call even when the path heuristic saw no
    // risky token. No provider = None = today's behavior exactly.
    let semantic = semantic_turn_consult(deps, handle, changed, cancel).await;
    let semantic_high = semantic
        .as_ref()
        .is_some_and(|state| semantic_risk_escalates(Some(state.level)));
    // 3. Merge the local structured findings into the verdict.
    let mut blocking = review_strings(review.get("blocking"));
    let mut suspects = review_strings(review.get("suspects"));
    for b in &evidence.blocking {
        review_push_str(&mut blocking, b.clone());
    }
    for s in &evidence.suspects {
        review_push_str(&mut suspects, s.clone());
    }
    // 3b. Audit 16 high-risk evidence gate: before this review relies on the
    //     per-file evidence, every changed file's CURRENT hash must equal the
    //     edit/checkpoint authority's expected hash. A mismatch is a TYPED
    //     refusal (blocking reason) — the change never completes on drifted
    //     bytes; the next attempt refreshes against the new state.
    if evidence.risk.level == faktor_verify::review::RiskLevel::High || semantic_high {
        if let Err(refusal) = require_edit_evidence_hashes(deps, handle, ws, changed) {
            tracing::warn!(
                session = %handle.id(),
                "high-risk review evidence refused: {refusal}"
            );
            review_push_str(&mut blocking, refusal.to_string());
        }
    }
    // 4. Risky changes get the REAL separate review call. NEVER skipped:
    //    RouterUnavailable → local signals stand (warned); every other
    //    failure or an oversized package → fail-closed blocking reason.
    let mut review_model = serde_json::json!({ "attempted": false });
    // Audit item 3: the live reviewer output (raw text + trust class + the
    // physical call's durable id) is captured so the final review value can
    // be stamped with its provenance before it leaves this function.
    let mut reviewer_output: Option<ModelOutput> = None;
    if evidence.risk.level == faktor_verify::review::RiskLevel::High || semantic_high {
        review_model["attempted"] = serde_json::json!(true);
        review_model["semantic_risk"] =
            serde_json::json!(semantic
                .as_ref()
                .map(|s| format!("{:?}", s.level).to_lowercase()));
        let outcome = match &evidence.package_json {
            Some(pkg) => {
                run_independent_review_call(
                    deps,
                    handle,
                    pkg,
                    &criteria,
                    semantic.as_ref().map(|s| s.level),
                    cancel,
                )
                .await
            }
            None => {
                let reason = evidence
                    .oversize
                    .clone()
                    .unwrap_or_else(|| "review package unavailable".into());
                IndependentReviewOutcome::refused(
                    "",
                    "",
                    format!("change too large for a structured independent review: {reason}"),
                )
            }
        };
        if let Some(verdict) = &outcome.verdict {
            match verdict.verdict {
                faktor_verify::review::ReviewVerdictKind::Clean => {}
                faktor_verify::review::ReviewVerdictKind::Concern => {
                    for f in &verdict.findings {
                        review_push_str(
                            &mut suspects,
                            format!("independent review model concern: {f}"),
                        );
                    }
                }
                faktor_verify::review::ReviewVerdictKind::Block => {
                    if verdict.findings.is_empty() {
                        review_push_str(
                            &mut blocking,
                            "independent review model blocked the change with no findings".into(),
                        );
                    } else {
                        for f in &verdict.findings {
                            review_push_str(
                                &mut blocking,
                                format!("independent review model: {f}"),
                            );
                        }
                    }
                }
            }
            review_model["status"] = serde_json::json!("called");
            review_model["verdict"] =
                serde_json::json!(format!("{:?}", verdict.verdict).to_lowercase());
            review_model["findings"] = serde_json::to_value(&verdict.findings).unwrap_or_default();
        } else if let Some(reason) = &outcome.refused {
            tracing::warn!(
                session = %handle.id(),
                "risky change review refused; the change must not complete unreviewed: {reason}"
            );
            review_push_str(
                &mut blocking,
                format!(
                    "independent review of a risky change could not run: {truncated}",
                    truncated = truncate(reason, 300)
                ),
            );
            review_model["status"] = serde_json::json!("refused");
            review_model["detail"] = serde_json::json!(truncate(reason, 300));
        }
        review_model["provider"] = serde_json::json!(outcome.provider);
        review_model["model"] = serde_json::json!(outcome.model);
        reviewer_output = outcome.output().cloned();
    }
    // 5. Write back the merged verdict + structured evidence.
    if let Some(obj) = review.as_object_mut() {
        obj.insert("blocking".into(), serde_json::json!(blocking));
        obj.insert("suspects".into(), serde_json::json!(suspects));
        let verdict = if blocking.is_empty() { "pass" } else { "block" };
        obj.insert("verdict".into(), serde_json::json!(verdict));
        if let Some(evidence_obj) = obj.get_mut("evidence").and_then(|e| e.as_object_mut()) {
            let mut structured = evidence.value;
            structured["review_model"] = review_model;
            if let Some(state) = &semantic {
                // Provider DATA (audit 49): the rendered block is already
                // `[evidence:data]`-tagged; the record stays record-bounded.
                structured["semantic"] = serde_json::json!({
                    "provider": state.provider,
                    "risk": format!("{:?}", state.level).to_lowercase(),
                    "evidence": state
                        .evidence
                        .first()
                        .map(|e| truncate(&e.snippet, 2048))
                        .unwrap_or_default(),
                });
            }
            if let Some(o) = &evidence.oversize {
                structured["oversize"] = serde_json::json!(o);
            }
            evidence_obj.insert("structured".into(), structured);
        }
    }
    // Audit item 3: stamp the review value's provenance BEFORE it crosses
    // out of this function. A live reviewer verdict is admitted through its
    // verification-opinion output (the same gate the criterion/proof
    // writers consume); a review with no reviewer output is
    // deterministic-local and must not claim a reviewer attempt. If neither
    // admission holds (a claimed reviewer attempt with no admitted output),
    // the value is returned UNSTAMPED so no consumer can ever admit it as
    // evidence — its blocking findings still gate the turn.
    let stamp = match &reviewer_output {
        Some(output) => ReviewEvidence::from_review_output(output, review.clone()),
        None => ReviewEvidence::from_deterministic_local(review.clone()),
    };
    match stamp {
        Ok(evidence) => Some(evidence.into_value()),
        Err(refusal) => {
            tracing::error!(
                session = %handle.id(),
                "completion review evidence refused by the output-trust gate: {refusal}"
            );
            Some(review)
        }
    }
}

/// String entries of a review array field (bounded, hostile-safe).
pub(crate) fn review_strings(v: Option<&serde_json::Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The deterministic source-snapshot id of one check-set derivation (audits
/// 56/57): the required checks (id + command, derivation order) folded with
/// the same stable FNV-1a 64 content hash the criteria ids use. A changed
/// check set yields a different snapshot id, which makes every derived
/// criterion of the old snapshot stale and forces a re-derivation.
pub(crate) fn criteria_derivation_snapshot(required: &[&faktor_verify::Check]) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for check in required {
        for bytes in [check.id.as_bytes(), check.command.as_bytes()] {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(PRIME);
            }
        }
        hash ^= 0x1f;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("required-checks:v1:{hash:016x}")
}

/// The canonical memory-fact text of the acceptance-criteria entries
/// (newline-joined, bounded to the durable fact cap like every fact value).
/// Byte-deterministic: the row's entries, the seeded `criteria`/`0` fact and
/// the restart re-seed all derive through this one function, so they agree
/// exactly and restart never rewrites an unchanged fact.
pub(crate) fn criteria_canonical_text(entries: &[String]) -> String {
    truncate(&entries.join("\n"), 3000)
}

/// True when the durable task row's spend already exceeds its budget caps
/// (a `None` max field is unlimited). The gate refuses VerifiedComplete for
/// such a task (audit 25).
pub(crate) fn task_budget_exhausted(t: &Task) -> bool {
    t.budget
        .max_tokens
        .is_some_and(|m| t.budget.spent_tokens > m)
        || t.budget.max_turns.is_some_and(|m| t.budget.spent_turns > m)
}

/// One required check that RAN in a verification attempt (typed migration
/// P0-9/10): the CheckOutcome data the durable-proof [`CheckExecution`] rows
/// are built from — real program/argv, status, exit, summary and
/// timestamps — so records never re-split a shell string.
#[derive(Debug, Clone)]
pub(crate) struct ExecutedCheck {
    /// Stable check id (the derived check's id, unchanged from wave-16).
    pub(crate) id: String,
    /// Legacy kind of the executed check (drives the record's category).
    pub(crate) kind: faktor_verify::CheckKind,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) passed: bool,
    pub(crate) exit: Option<i32>,
    pub(crate) summary: Option<String>,
    pub(crate) started_ms: i64,
    pub(crate) finished_ms: i64,
}

/// Turn one executed typed check + outcome into its proof row.
pub(crate) fn executed_check_row(
    check: &faktor_verify::Check,
    spec: &faktor_verify::exec::CheckSpec,
    outcome: &faktor_verify::exec::CheckOutcome,
) -> ExecutedCheck {
    ExecutedCheck {
        id: check.id.clone(),
        kind: check.kind,
        program: spec.program.to_string_lossy().into_owned(),
        args: spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        passed: outcome.status == faktor_verify::exec::CheckRunStatus::Passed,
        exit: outcome.exit,
        summary: outcome.summary.clone(),
        started_ms: outcome.started_ms,
        finished_ms: outcome.finished_ms,
    }
}

/// One durable attempt's COMPLETE required-check picture, rebuilt from its
/// ordered attempt record (inline outcomes frozen at enqueue) plus its job
/// rows (background outcomes resolved by an executor). Shared by the turn
/// settlement and the attempt-based integrated-root verification so the two
/// settlement paths can never reconstruct an attempt differently.
pub(crate) struct AttemptRebuild {
    pub(crate) mirrors: Vec<faktor_verify::Check>,
    pub(crate) results: Vec<(String, bool)>,
    pub(crate) unavailable: Vec<(String, String)>,
    pub(crate) executed: Vec<ExecutedCheck>,
}

/// The legacy [`faktor_verify::Check`] mirror of a typed spec (builder
/// families, wave 17): the SAME id/kind/required semantics with the
/// canonical command text (`program arg...` — simple tokens by
/// construction, deterministic join). Criteria rows, durable facts, gate
/// reasons and proof records consume the mirror; the typed spec is what
/// executes.
pub(crate) fn legacy_mirror_of_spec(spec: &faktor_verify::exec::CheckSpec) -> faktor_verify::Check {
    let kind = match spec.kind {
        faktor_verify::exec::CheckKind::Compile => faktor_verify::CheckKind::Compile,
        faktor_verify::exec::CheckKind::Test => faktor_verify::CheckKind::Test,
        faktor_verify::exec::CheckKind::Lint => faktor_verify::CheckKind::Lint,
    };
    let mut command = spec.program.to_string_lossy().into_owned();
    for arg in &spec.args {
        command.push(' ');
        command.push_str(&arg.to_string_lossy());
    }
    faktor_verify::Check {
        id: spec.id.clone(),
        kind,
        command,
        affects: spec.affects.clone(),
        required: spec.required,
    }
}

// ------------------------------------------- environment fingerprint basis

/// Manifest files whose content hashes form the fingerprint basis (bounded,
/// fixed order). A missing file is omitted — an absent manifest is an honest
/// absence, never a zero or guessed hash.
pub(crate) const FINGERPRINT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "CMakeLists.txt",
    "meson.build",
    "BUILD",
];

/// Lockfiles whose content hashes form the fingerprint basis (bounded, fixed
/// order; same absence rule as the manifests).
pub(crate) const FINGERPRINT_LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "requirements.txt",
    "go.sum",
    "gradle.lockfile",
    "composer.lock",
    "Gemfile.lock",
];

/// The ONLY process environment projection the check-basis hash observes
/// (fixed order): verification-relevant build variables. The full process
/// environment is deliberately never captured — secrets and unrelated
/// variables stay out of the digest.
pub(crate) const FINGERPRINT_ENV_KEYS: &[&str] = &[
    "RUSTUP_TOOLCHAIN",
    "RUSTFLAGS",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET",
];

/// The verification implementation version stamped on every fingerprint:
/// this build of the agent that executed the checks.
pub const VERIFICATION_IMPL_VERSION: &str = concat!("faktor-agent/", env!("CARGO_PKG_VERSION"));

/// The known manifest + lockfile hashes present at the verification root,
/// streamed whole-file through the workspace handle (bounded memory; a file
/// the handle refuses — absent, escaping, unreadable — is omitted).
pub(crate) fn fingerprint_build_inputs(
    workspace: Option<&faktor_fs::WorkspaceHandle>,
) -> (Vec<FingerprintFileHash>, Vec<FingerprintFileHash>) {
    let mut manifests = Vec::new();
    let mut lockfiles = Vec::new();
    let Some(ws) = workspace else {
        return (manifests, lockfiles);
    };
    for rel in FINGERPRINT_MANIFESTS {
        if let Ok((_, hash)) = ws.hash_file_streaming(std::path::Path::new(rel), None) {
            manifests.push(FingerprintFileHash {
                path: (*rel).to_string(),
                digest_hex: hash.to_hex(),
            });
        }
    }
    for rel in FINGERPRINT_LOCKFILES {
        if let Ok((_, hash)) = ws.hash_file_streaming(std::path::Path::new(rel), None) {
            lockfiles.push(FingerprintFileHash {
                path: (*rel).to_string(),
                digest_hex: hash.to_hex(),
            });
        }
    }
    (manifests, lockfiles)
}

/// BLAKE3 (length-prefixed) aggregate over an ordered fingerprint-file list:
/// the candidate/base manifest hash. An empty list still yields the stable
/// hash of the empty input — never a placeholder string.
pub(crate) fn fingerprint_file_aggregate(entries: &[FingerprintFileHash]) -> String {
    let mut hasher = blake3::Hasher::new();
    for e in entries {
        for part in [e.path.as_bytes(), e.digest_hex.as_bytes()] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// BLAKE3 (length-prefixed) digest of the ordered check basis: every
/// `(id, program, argv)` in derivation order, the root-relative cwd (`"."` —
/// every typed check runs under the verification root) and the fixed
/// environment allowlist projection. Identical check vectors in identical
/// environments produce the identical digest.
pub(crate) fn fingerprint_check_basis(checks: &[(String, String, Vec<String>)]) -> String {
    let mut hasher = blake3::Hasher::new();
    let mut part = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    for (id, program, args) in checks {
        part(b"check");
        part(id.as_bytes());
        part(program.as_bytes());
        part(b"cwd:.");
        part(&(args.len() as u64).to_le_bytes());
        for arg in args {
            part(arg.as_bytes());
        }
    }
    for key in FINGERPRINT_ENV_KEYS {
        part(key.as_bytes());
        match std::env::var(key) {
            Ok(value) if !value.is_empty() => part(value.as_bytes()),
            _ => part(b"<absent>"),
        }
    }
    hasher.finalize().to_hex().to_string()
}

/// BLAKE3 of the typed V2 criteria JSON of the task row — the task-contract
/// basis. A missing/unreadable task row hashes the empty criteria set
/// (deterministically), never an error: the fingerprint must never block the
/// record the completion path depends on.
pub(crate) fn fingerprint_task_contract(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
) -> String {
    let criteria = match handle.get_task(task_id) {
        Ok(Some(task)) => task.criteria(),
        _ => Vec::new(),
    };
    match serde_json::to_vec(&criteria) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
        Err(_) => blake3::hash(b"criteria-unavailable").to_hex().to_string(),
    }
}

// ------------------------------------------- typed criterion evaluation (P0)
//
// The unsound `passed = suite passed` mapping is GONE: every acceptance
// criterion is evaluated through its OWN typed binding. The evaluator
// implementation lives in `faktor_verify::criteria`; the runtime supplies
// the read-only ports (candidate repo, observed evidence, the recorded
// independent review) and NEVER a mutation surface.

/// Read-only candidate repo: whole-file streaming hashes through a cloned
/// workspace handle (never a mutation).
pub(crate) struct CandidateRepo(pub(crate) faktor_fs::WorkspaceHandle);

/// Best-effort canonical tree-manifest digest of the verification root
/// (`tm1:<64-hex>`, the ONE tree identity shared with run bases, candidates,
/// integration records and completion gates). An unreadable/oversized root —
/// or one carrying special files, where tree equality is unprovable — yields
/// the honest `snapshot-unavailable` id, never a guessed digest.
pub(crate) fn root_snapshot_best_effort(ws: &faktor_fs::WorkspaceHandle) -> String {
    faktor_fs::tree_manifest::tree_manifest_digest(
        ws.root(),
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .unwrap_or_else(|_| "snapshot-unavailable".into())
}

// --------------------------------------------------------------------------
// Lazy tool activation (docs/acquire.md §2/§4): the runtime half of the
// deterministic activation policy. The session's activation set lives in
// durable memory facts (legacy `kind`/`key` rows, which the runtime's typed
// memory DATA block deliberately hides), so it survives restarts and adds
// ZERO context tokens on every turn — including activated ones.
// --------------------------------------------------------------------------

/// Durable memory-fact kind of the session's lazy-tool activation set.
pub(crate) const TOOL_ACTIVATION_FACT_KIND: &str = "tool_activation";

/// Durable memory-fact key of the set (the value is a JSON name array).
pub(crate) const TOOL_ACTIVATION_FACT_KEY: &str = "set";

/// Newest-first page size of the bounded durable scan.
pub(crate) const TOOL_ACTIVATION_PAGE: i64 = 200;

/// Bounded page walk of the durable scan: a missed flag degrades to the
/// in-turn deterministic detection, never to a wrong activation. Exhausting
/// this bound without a decisive fact is diagnosed LOUDLY and falls back to
/// the safe inactive default (never a silent revert).
pub(crate) const TOOL_ACTIVATION_MAX_PAGES: usize = 16;

impl AgentRuntime {
    pub fn new(deps: AgentDeps) -> faktor_core::Result<Arc<Self>> {
        if deps.model.is_empty() {
            return Err(Error::malformed("agent requires a model"));
        }
        // THE durable evidence authority roots beside the session store, so
        // every daemon over the same data directory shares the same evidence
        // identity space across restarts.
        let evidence_authority = Arc::new(DurableEvidenceAuthority::for_store(
            deps.session.store(),
            EVIDENCE_BACKING_CAP_BYTES,
        ));
        Ok(Arc::new(Self {
            deps: Arc::new(deps),
            runners: std::sync::Mutex::new(std::collections::HashMap::new()),
            progress: std::sync::Mutex::new(std::collections::HashMap::new()),
            coordination: std::sync::Mutex::new(std::collections::HashMap::new()),
            token_cache: TokenCache::new(),
            stall_silence_ms: std::sync::atomic::AtomicU64::new(DEFAULT_STALL_SILENCE_MS),
            quality_mode: std::sync::atomic::AtomicU8::new(0),
            index_service: std::sync::OnceLock::new(),
            evidence_authority,
            provider_debits: std::sync::Mutex::new(None),
            retry_policy_override: std::sync::Mutex::new(None),
        }))
    }

    /// THE durable evidence authority of this runtime (schema v21).
    pub fn evidence_authority(&self) -> &Arc<DurableEvidenceAuthority> {
        &self.evidence_authority
    }

    pub fn deps(&self) -> &AgentDeps {
        &self.deps
    }

    /// A changed turn that no objective mechanism could verify: classify
    /// Unverified, write the durable rows (`task_state` NeedsVerification +
    /// `verification` last-run Unavailable) and warn — mutating turns
    /// without a running verifier are NEVER silently 'completed'.
    pub(crate) fn unverified_verdict(
        &self,
        handle: &faktor_session::SessionHandle,
        changed: &[String],
        review: Option<serde_json::Value>,
        reason: &str,
    ) -> TurnEndVerdict {
        tracing::warn!(
            "session {}: completion gate Unverified — {}; {} file(s) changed; \
             the change is NOT verified complete",
            handle.id(),
            reason,
            changed.len()
        );
        self.persist_gate_facts(
            handle,
            &CompletionGate::Unverified,
            VerificationStatus::Unavailable,
            &[],
            changed,
            None,
        );
        TurnEndVerdict {
            verification: Vec::new(),
            acceptance: None,
            review,
            completion: Some(CompletionGate::Unverified),
            criteria: None,
            proof: None,
        }
    }

    /// Archive one large tool output through the durable evidence authority
    /// (CCR): normalized, compressed, backing retrievable. Returns the
    /// bounded reference line that replaces the raw output on the wire, or
    /// `None` when the output is small / the flag is off / archiving failed.
    pub(crate) fn archive_tool_output(
        &self,
        handle: &faktor_session::SessionHandle,
        task_id: TaskId,
        tool: &str,
        provenance: ProvenanceSource,
        raw: &str,
    ) -> Option<String> {
        if !self.deps.efficiency.ccr || raw.len() < TOOL_OUTPUT_EVIDENCE_MIN_BYTES {
            return None;
        }
        let workspace_id = handle
            .identity()
            .map(|i| i.workspace_id)
            .unwrap_or_else(|_| WorkspaceId::new(1));
        match self.evidence_authority.archive_text(
            handle.id(),
            workspace_id,
            Some(task_id.raw()),
            tool_evidence_kind(tool),
            Some(tool),
            provenance,
            raw,
            faktor_context::compactor::EVIDENCE_COMPACT_BODY_MAX_BYTES,
        ) {
            Ok(envelope) => Some(format!(
                "[evidence://{} kind={:?} original_bytes={} compact_bytes={}]",
                envelope.id,
                envelope.kind,
                envelope.compression.original_bytes,
                envelope.compression.compact_bytes,
            )),
            Err(err) => {
                tracing::warn!(
                    session = %handle.id(),
                    tool = %tool,
                    "large tool output archive failed: {err}"
                );
                None
            }
        }
    }

    /// Configure the layered per-logical-turn wall-clock budget (audit 26)
    /// on every session this runtime drives: each drive is capped at one
    /// `turn_budget_ms` slice; the task itself spans unbounded wall-clock
    /// across slices and is bounded only by its durable token/turn budget.
    /// Delegates to the session manager so the prompt operation's deadline
    /// and the runtime's slice ceiling always agree. 0 = unbounded per turn.
    pub fn set_turn_budget_ms(&self, ms: u64) {
        self.deps.session.set_turn_budget_ms(ms);
    }

    /// The per-session bounded progress record (created on first touch).
    pub(crate) fn with_tracker<T>(
        &self,
        session: SessionId,
        f: impl FnOnce(&mut StallTracker) -> T,
    ) -> T {
        let mut map = self.progress.lock().unwrap();
        let threshold = self.stall_silence();
        f(map
            .entry(session)
            .or_insert_with(|| StallTracker::new(threshold)))
    }

    /// Session-scoped lifecycle hook dispatch (audit): runs the wired hook
    /// registry for [`faktor_hooks::HookEvent::SessionStart`],
    /// [`faktor_hooks::HookEvent::SessionResume`] or
    /// [`faktor_hooks::HookEvent::SessionEnd`] with the session id as the
    /// `{session_id}` payload. Best-effort and audit-only: a Deny verdict on
    /// a lifecycle event is logged and recorded in the registry audit log —
    /// it NEVER fails the session transition or rolls the session back.
    ///
    /// Sessions are CREATED by the session manager (server side), not by the
    /// runtime, so SessionStart is not fired inside this file: every session
    /// creator (the ACP daemon entry, `faktor run`, the native
    /// `POST /native/session` handler and the worker node) calls this helper
    /// right after `create_session` succeeds, before the session is first
    /// used. SessionEnd fires from
    /// [`AgentRuntime::end_session`]. SessionResume fires from
    /// [`AgentRuntime::continue_record`] — the ONLY recovery-resume
    /// boundary; `drive_receipt` also runs after `recover_session` for
    /// brand-new prompts, which is not a resume, so it never fires there.
    pub fn run_lifecycle_hook(&self, event: faktor_hooks::HookEvent, session: SessionId) {
        match event {
            faktor_hooks::HookEvent::SessionStart
            | faktor_hooks::HookEvent::SessionResume
            | faktor_hooks::HookEvent::SessionEnd => {}
            other => {
                tracing::warn!(
                    "run_lifecycle_hook only dispatches session events; ignoring {other:?}"
                );
                return;
            }
        }
        self.run_hook_best_effort(
            event,
            session,
            None,
            serde_json::json!({ "session_id": session.to_string() }),
        );
    }

    /// SessionStart lifecycle hook for one newly created durable session:
    /// the ONE seam every session creator calls immediately after
    /// `create_session` succeeds and before the session is first used (the
    /// same ordering the ACP daemon entry established). Best-effort and
    /// audit-only — a failing/hanging hook is bounded by its registry
    /// deadline and can NEVER fail or roll back the session creation, and a
    /// registry-less runtime is a no-op.
    pub fn run_session_start_hook(&self, session: SessionId) {
        self.run_lifecycle_hook(faktor_hooks::HookEvent::SessionStart, session);
    }

    /// Best-effort post-hoc hook dispatch: run every registry hook
    /// registered for `event`. A verdict AFTER the fact is audit-only — the
    /// registry writes its own audit record for every run, and a Deny here
    /// is logged but NEVER retroactively fails the turn/session (Warn/Allow
    /// pass silently). Missing registry: no-op.
    pub(crate) fn run_hook_best_effort(
        &self,
        event: faktor_hooks::HookEvent,
        session: SessionId,
        op_id: Option<OpId>,
        payload: serde_json::Value,
    ) {
        let Some(hooks) = &self.deps.hooks else {
            return;
        };
        let input = faktor_hooks::HookInput {
            event,
            session_id: Some(session.to_string()),
            task_id: None,
            operation_id: op_id.map(|o| o.to_string()),
            // P1: the anchored session workspace, never the daemon cwd. A
            // store failure here leaves the root None and the hook refuses
            // typed (audit-only path).
            workspace_root: self
                .deps
                .session
                .resolve_workspace_root(session)
                .ok()
                .flatten(),
            payload,
        };
        if let faktor_hooks::HookVerdict::Deny { reason } = hooks.run(event, &input) {
            tracing::warn!(
                "hook event {event:?} on session {session} denied after the fact (audit-only): {reason}"
            );
        }
    }

    /// TaskComplete lifecycle hook (audit): fired at the TWO genuine
    /// end-of-turn sites, right after `TurnCompleted` is journaled, with
    /// the final state and the turn's own verification/review evidence.
    /// Best-effort + audit-only — a Deny can never un-complete a turn.
    pub(crate) fn fire_task_complete_hook(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        outcome: &TurnOutcome,
    ) {
        self.run_hook_best_effort(
            faktor_hooks::HookEvent::TaskComplete,
            handle.id(),
            Some(op_id),
            serde_json::json!({
                "finalState": outcome.final_state,
                "verification": outcome.verification,
                "review": outcome.review,
            }),
        );
    }

    // ------------------------------------------------------------ entry points

    /// Seed the run's durable BINARY attachment set into one child session
    /// BEFORE its drive begins (the orchestrated-child path; the parent's
    /// rows are session-scoped, so each id is re-admitted here):
    ///
    /// - the terminal preflight runs FIRST: a child whose task row is already
    ///   terminal is frozen and returns `Ok` with NO attachment write at all
    ///   (the row cannot gain new durable references);
    /// - every id is structurally validated and every CAS blob is verified
    ///   by a streamed re-hash BEFORE anything is written — no bytes
    ///   materialize and a missing/tampered blob leaves zero new rows;
    /// - the reference rows AND the task row write (create or patch) land in
    ///   ONE store `BEGIN IMMEDIATE` transaction
    ///   ([`faktor_session::SessionHandle::seed_task_attachments`]): every
    ///   failed condition rolls the whole seed back, so a failure leaves ZERO
    ///   durable writes. There is no two-transaction window in which the
    ///   complete reference set can be orphaned by a failed task patch, and a
    ///   task transitioned to terminal between the preflight and the write is
    ///   re-verified inside the transaction (typed refusal, zero writes) —
    ///   never a frozen row that gained references.
    ///
    /// An empty set is a no-op (byte-parity for attachment-free children).
    pub fn seed_task_attachments(
        &self,
        session: SessionId,
        attachments: &[faktor_core::attachment::AttachmentId],
    ) -> faktor_core::Result<()> {
        if attachments.is_empty() {
            return Ok(());
        }
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        let task_id = handle.task_id()?;
        let existing = handle.get_task(task_id)?;
        if existing
            .as_ref()
            .is_some_and(|task| task.state.is_terminal())
        {
            return Ok(());
        }
        // The create template is used ONLY when the preflight saw no task row;
        // an existing row is patched under the transaction's revision CAS.
        let create = if existing.is_none() {
            let now = handle.now_ms();
            let goal = truncate(&handle.title()?, 200);
            Some(Task {
                task_id,
                session_id: session,
                goal,
                acceptance_criteria: Vec::new(),
                plan: Vec::new(),
                attachments: Vec::new(),
                budget: faktor_session::TaskBudget::default(),
                state: TaskState::Pending,
                created_ms: now,
                updated_ms: now,
            })
        } else {
            None
        };
        handle.seed_task_attachments(attachments, create)?;
        Ok(())
    }

    pub(crate) fn last_event_kind(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Option<faktor_core::event::EventKind> {
        let last = handle.last_event_seq().ok()??;
        let n = last.raw();
        handle
            .events_range(n.saturating_sub(1).max(1), Some(2))
            .ok()?
            .into_iter()
            .find(|e| e.seq == last)
            .map(|e| e.kind)
    }

    /// Resolve the instruction epoch of the session's DURABLE workspace
    /// root through the per-workspace resolver (P0-32): `(epoch,
    /// rules_present)`. `None` when the session has no durable workspace
    /// root (documented Empty result — the ledger simply records no epoch,
    /// exactly like an unwired loader did). A hostile tree
    /// (oversized/unreadable authority rule file) is a typed resolver
    /// error, surfaced as a warn — the ledger never records an epoch it
    /// cannot verify.
    pub(crate) fn session_instruction_epoch(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> Option<(u64, bool)> {
        let row = handle.row().ok()?;
        match self
            .deps
            .instructions_resolver
            .resolve(row.workspace_id.raw(), None)
        {
            Ok(loaded) => loaded.epoch().map(|e| (e.as_u64(), loaded.has_rules())),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "workspace instructions resolve failed; no epoch row recorded"
                );
                None
            }
        }
    }

    /// Layered lifetime slice probe (audit 26): true when the configured
    /// per-turn wall-clock budget has elapsed since this drive started
    /// (0 = unbounded, the operator opted out of the wall-clock turn cap).
    pub(crate) fn slice_expired(
        &self,
        handle: &faktor_session::SessionHandle,
        slice_started_ms: i64,
    ) -> bool {
        let budget = handle.turn_budget_ms();
        if budget == 0 {
            return false;
        }
        self.deps.clock.now_ms().saturating_sub(slice_started_ms) >= budget as i64
    }

    /// Stop and JOIN the lazily-hosted repository `IndexService` worker, if
    /// the service was ever opened. `None` = the service was never requested
    /// — this accessor NEVER opens it (no side effect) — so an embedder that
    /// never ran a turn has nothing to join. `Some(outcome)` mirrors
    /// [`faktor_index::IndexService::shutdown_worker`] exactly:
    /// [`WorkerShutdown::NotRunning`](faktor_index::WorkerShutdown::NotRunning)
    /// when no owned task was alive (never spawned, or already stopped/failed
    /// — the idempotent repeat call),
    /// [`Joined`](faktor_index::WorkerShutdown::Joined) when the owned task
    /// was cancelled and joined within the service's own bound, and
    /// [`Aborted`](faktor_index::WorkerShutdown::Aborted) when it exceeded
    /// that bound, was aborted, and its retained handle was still awaited —
    /// nothing is ever detached. An in-flight blocking pass cannot be
    /// force-killed: it observes cancellation at its next workspace boundary
    /// and is bounded by the scan caps + build lease.
    ///
    /// This is THE join point of the runtime for the index worker: the
    /// runtime has no async teardown and its `Drop` cannot await (the
    /// service exposes no synchronous cancel), so a host — the daemon — must
    /// call this during its own shutdown sequence, before dropping the
    /// runtime. Safe and bounded when the worker was never started or is
    /// already stopped.
    pub async fn shutdown_index_service(&self) -> Option<faktor_index::WorkerShutdown> {
        let service = self
            .index_service
            .get()
            .and_then(|service| service.clone())?;
        Some(service.shutdown_worker().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::{deps, new_session};
    use faktor_core::model::ModelCapabilities;
    use faktor_provider::FakeProvider;

    /// A parent session (the upload owner) and a distinct, EMPTY child
    /// session (the seed target) over one runtime, mirroring the real
    /// orchestrated-child path.
    fn parent_child() -> (
        std::sync::Arc<AgentRuntime>,
        faktor_session::SessionHandle,
        faktor_session::SessionHandle,
        tempfile::TempDir,
    ) {
        let (deps, dir) = deps(
            FakeProvider::new("fake", ModelCapabilities::default()),
            Vec::new(),
        );
        let runtime = AgentRuntime::new(deps).unwrap();
        let parent = new_session(runtime.deps());
        let child = new_session(runtime.deps());
        let parent_handle = runtime.deps.session.get_session(parent).unwrap().unwrap();
        let child_handle = runtime.deps.session.get_session(child).unwrap().unwrap();
        (runtime, parent_handle, child_handle, dir)
    }

    fn seed_task_row(
        handle: &faktor_session::SessionHandle,
        store: &faktor_store::Store,
        session: SessionId,
        state: TaskState,
        revision: u64,
    ) {
        let now = handle.now_ms();
        store
            .upsert_task(&faktor_store::TaskRow {
                task_id: handle.task_id().unwrap(),
                session_id: session,
                goal: "seed target".into(),
                acceptance_criteria: Vec::new(),
                plan: Vec::new(),
                attachments: Vec::new(),
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state,
                revision: faktor_core::id::TaskRevision::new(revision),
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
    }

    fn sorted(ids: &mut [faktor_core::attachment::AttachmentId]) {
        ids.sort_by_key(|id| id.digest.to_hex());
    }

    /// A terminal task row is frozen: seeding new attachments returns `Ok`
    /// and writes NOTHING (no reference rows, no task patch).
    #[test]
    fn seed_task_attachments_terminal_task_is_frozen_with_no_writes() {
        let (runtime, parent, child, _dir) = parent_child();
        let attachment = parent
            .put_attachment("image/png", Some("late.png"), b"\x89PNG-late")
            .unwrap();
        let task_id = child.task_id().unwrap();
        let now = child.now_ms();
        child
            .create_task(Task {
                task_id,
                session_id: child.id(),
                goal: "frozen".into(),
                acceptance_criteria: Vec::new(),
                plan: Vec::new(),
                attachments: Vec::new(),
                budget: Default::default(),
                state: TaskState::Pending,
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
        child
            .transition_task(
                task_id,
                child.task_revision(task_id).unwrap(),
                TaskTransition::Cancel,
                None,
            )
            .unwrap();
        assert!(
            child.list_attachments(16).unwrap().is_empty(),
            "the child starts without any attachment row"
        );
        runtime
            .seed_task_attachments(child.id(), std::slice::from_ref(&attachment))
            .expect("a terminal task is frozen, not an error");
        assert!(
            child.list_attachments(16).unwrap().is_empty(),
            "a terminal task must gain zero new attachment reference rows"
        );
        let task = child.get_task(task_id).unwrap().unwrap();
        assert!(task.state.is_terminal());
        assert_eq!(task.attachments, Vec::new());
        // The parent's own reference is untouched.
        assert_eq!(parent.list_attachments(16).unwrap(), vec![attachment]);
    }

    /// `[A valid, B missing]` leaves NEITHER A nor B durable in the child:
    /// every CAS blob is verified before the first reference row is written.
    #[test]
    fn seed_task_attachments_missing_second_blob_adds_neither() {
        let (runtime, parent, child, _dir) = parent_child();
        let valid = parent
            .put_attachment("image/png", Some("a.png"), b"\x89PNG-a")
            .unwrap();
        let missing = faktor_core::attachment::AttachmentId {
            digest: faktor_core::hash::FileHash::from([99; 32]),
            ..valid.clone()
        };
        let err = runtime
            .seed_task_attachments(child.id(), &[valid.clone(), missing])
            .expect_err("a missing blob must refuse the whole seed");
        assert_eq!(err.kind, ErrorKind::NotFound);
        assert!(
            child.list_attachments(16).unwrap().is_empty(),
            "neither A nor B may be added when one blob is missing"
        );
        assert!(
            child.get_task(child.task_id().unwrap()).unwrap().is_none(),
            "no task row may be created for a refused seed"
        );
        // The refused A was never recorded in the child: an exact-resolution
        // attempt is an honest absence, and a later lawful seed still works.
        assert_eq!(child.attachment(valid.digest).unwrap(), None);
        runtime
            .seed_task_attachments(child.id(), std::slice::from_ref(&valid))
            .unwrap();
        assert_eq!(child.list_attachments(16).unwrap(), vec![valid.clone()]);
        let task = child.get_task(child.task_id().unwrap()).unwrap().unwrap();
        assert_eq!(task.attachments, vec![valid]);
    }

    /// The success path lands BOTH references and the task row's attachment
    /// set, and re-seeding converges idempotently.
    #[test]
    fn seed_task_attachments_lands_both_references_and_the_task_set() {
        let (runtime, parent, child, _dir) = parent_child();
        let a = parent
            .put_attachment("image/png", Some("a.png"), b"\x89PNG-a")
            .unwrap();
        let b = parent
            .put_attachment("application/pdf", Some("b.pdf"), b"%PDF-b")
            .unwrap();
        runtime
            .seed_task_attachments(child.id(), &[a.clone(), b.clone()])
            .unwrap();
        let mut stored = child.list_attachments(16).unwrap();
        sorted(&mut stored);
        let mut expected = vec![a.clone(), b.clone()];
        sorted(&mut expected);
        assert_eq!(stored, expected, "both references are durable in the child");
        let task_id = child.task_id().unwrap();
        assert_eq!(
            child.get_task(task_id).unwrap().unwrap().attachments,
            vec![a.clone(), b.clone()],
            "the task row carries the exact set"
        );
        // Idempotent re-seed (crash re-attach): same refs, same task set.
        runtime
            .seed_task_attachments(child.id(), &[a.clone(), b.clone()])
            .unwrap();
        assert_eq!(
            child.get_task(task_id).unwrap().unwrap().attachments,
            vec![a, b]
        );
    }

    /// A seed whose task patch fails leaves ZERO durable writes: the hostile
    /// revision-overflow row forces a typed Malformed before enqueue, and
    /// because the reference batch and the task write are ONE transaction
    /// there is no "complete reference set inserted, task row unpatched"
    /// intermediate. The task row is unchanged and the child holds no
    /// reference rows.
    #[test]
    fn seed_task_attachments_task_patch_failure_leaves_zero_writes() {
        let (runtime, parent, child, _dir) = parent_child();
        let a = parent
            .put_attachment("image/png", Some("a.png"), b"\x89PNG-a")
            .unwrap();
        let b = parent
            .put_attachment("application/pdf", Some("b.pdf"), b"%PDF-b")
            .unwrap();
        // Hostile durable row: non-terminal but with an exhausted revision,
        // so the seed must fail with a typed Malformed.
        seed_task_row(
            &child,
            &runtime.deps().session.store(),
            child.id(),
            TaskState::Running,
            u64::MAX,
        );
        let task_id = child.task_id().unwrap();
        let err = runtime
            .seed_task_attachments(child.id(), &[a.clone(), b.clone()])
            .expect_err("an exhausted revision must fail the seed");
        assert_eq!(err.kind, ErrorKind::Malformed);
        assert!(
            child.list_attachments(16).unwrap().is_empty(),
            "a failed seed must leave ZERO reference rows, never the complete set"
        );
        assert_eq!(
            child.get_task(task_id).unwrap().unwrap().attachments,
            Vec::new(),
            "the failed seed must not have changed the task row"
        );
    }
}
