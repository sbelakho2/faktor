//! `runtime::turn::drive_phases`: the per-`AgentState` phase bodies of the
//! logical-turn loop, mechanically moved out of
//! [`AgentRuntime::drive_turn_inner`] (same statements, same order, same
//! semantics; the loop-carried state is hoisted into [`TurnDriveState`]).
//!
//! @authority-collapse: authority-bearing
//!
//! The phases follow the machine: boundary (cancel check, durable child
//! controls, slice expiry, `BuildingContext` hop), context (`BuildingContext`:
//! retrieval + the turn's wire plan), route (planning: compaction + the one
//! economic route consult), the provider call (`WaitingForModel`/`Streaming`
//! in `drive_call.rs`) and the tool batch + genuine end (`ExecutingTool` into
//! `Validating`/`UpdatingMemory`).

use super::*;

/// The ONE capability view of a (provider, model) pair: the raw declared
/// profile, its AUTHORITY state, and the budget-ceiling caps. The effective
/// caps are authority-gated ([`faktor_core::model::CapabilityResolution::effective`])
/// — an `Unknown` lookup never silently enables model features or the
/// optimistic context — and then clamped by the provider's live runtime
/// window (the Ollama `/api/ps` allocation). Used at plan time AND at every
/// post-routing refresh, so the gate can never be bypassed through a
/// re-plan. A gated lookup is surfaced as a bounded warn (telemetry), never
/// silently.
pub(crate) fn resolve_turn_caps(
    provider: &std::sync::Arc<dyn faktor_provider::Provider>,
    model: &str,
) -> (
    faktor_core::model::ModelCapabilities,
    faktor_core::model::CapabilityAuthority,
    faktor_core::model::ModelCapabilities,
) {
    let resolution = provider.capability_resolution(model);
    let mut effective = resolution.effective();
    if let Some(limit) = provider.runtime_context_limit(model) {
        effective.context = effective.context.min(limit);
    }
    if resolution.is_gated() {
        tracing::warn!(
            provider = %provider.id(),
            model = %model,
            authority = resolution.authority.as_str(),
            "capability lookup carries no model-specific authority: model features are gated off until the endpoint declares this model (context/tools/vision conservative)"
        );
    }
    (resolution.caps, resolution.authority, effective)
}

/// Apply the durable per-session context-window override (F046). It can only
/// LOWER the effective budget below the model/runtime window; a value above
/// the window is clamped, never trusted to raise it.
pub(crate) fn apply_context_window_override(
    handle: &faktor_session::SessionHandle,
    mut effective: faktor_core::model::ModelCapabilities,
) -> faktor_core::model::ModelCapabilities {
    let Ok(Some(raw)) = handle.memory_fact_get("session_settings", "contextWindow") else {
        return effective;
    };
    let Ok(tokens) = raw.trim().parse::<u64>() else {
        return effective;
    };
    if tokens == 0 {
        return effective;
    }
    let Ok(tokens) = usize::try_from(tokens) else {
        return effective;
    };
    effective.context = effective.context.min(tokens);
    effective
}

/// Loop-carried state of ONE logical turn (the locals `drive_turn_inner`
/// kept across its iterations, lifted into one owner).
pub(crate) struct TurnDriveState<'a> {
    pub(crate) handle: &'a faktor_session::SessionHandle,
    pub(crate) op_id: OpId,
    pub(crate) cancel: CancellationToken,
    pub(crate) task_id: TaskId,
    pub(crate) outcome: TurnOutcome,
    pub(crate) turn_summary: faktor_context::ledger::TurnSummary,
    pub(crate) detector: LoopDetector,
    pub(crate) ledger: TaskLedger,
    pub(crate) settled_calls: Vec<(SettledCallOutcome, u8)>,
    pub(crate) drive_semantic_risk: u8,
    pub(crate) slice_started_ms: i64,
    pub(crate) provider: Arc<dyn faktor_provider::Provider>,
    pub(crate) routed_decision: Option<RouteDecision>,
    pub(crate) override_active: bool,
    pub(crate) model: String,
    pub(crate) envelope_fixed: bool,
    pub(crate) caps: faktor_core::model::ModelCapabilities,
    /// The authority state of `caps` (`known` / `declared_generic` /
    /// `unknown`): surfaced to telemetry and tests; `effective_caps` is the
    /// gated decision profile derived from it.
    pub(crate) cap_authority: faktor_core::model::CapabilityAuthority,
    pub(crate) effective_caps: faktor_core::model::ModelCapabilities,
    pub(crate) budget: ContextBudget,
    pub(crate) admitted_evidence_hash: Option<u64>,
    pub(crate) semantic_turn: Option<SemanticTurnState>,
    pub(crate) evidence_poll_status: Option<crate::EvidencePollStatus>,
    pub(crate) system_extra: String,
    /// The Faktor-rendered internal-context block of THIS turn
    /// ([`ParentTurnInput`] channel): DATA authority only — folded into the
    /// volatile system slot at every boundary, never into the conversation.
    /// Consumed exactly once from the runtime's op-keyed map by
    /// [`TurnDriveState::new`].
    pub(crate) internal_context: String,
    pub(crate) recent: Vec<RecentTurn>,
    /// Durable message-seq high-water mark of the loaded context window.
    /// An accepted compaction records this in the context checkpoint, so a
    /// restart replays the checkpoint + newer rows instead of the summarized
    /// transcript.
    pub(crate) recent_head_seq: Option<u64>,
    pub(crate) evidence: Vec<Evidence>,
    pub(crate) project_rules: String,
    pub(crate) repo_map: String,
    pub(crate) history: Vec<RequestMessage>,
    pub(crate) tool_bundle: Option<ToolBundle>,
    pub(crate) wire_plan: Option<WirePlan>,
    pub(crate) tool_calls: Vec<(String, String, serde_json::Value)>,
    /// Bounded in-turn retries of a failed MUTATION batch (stale hash,
    /// malformed operation, missing file). A recoverable tool error is not
    /// task completion; the model gets to see the typed failure and correct
    /// its arguments. The loop detector still ends a repeated identical
    /// failure, so this cannot spin.
    pub(crate) tool_retry_rounds: u8,
    pub(crate) assistant_message: Option<i64>,
    pub(crate) text_buf: String,
    pub(crate) reasoning_buf: String,
    pub(crate) tokens_in: u64,
    pub(crate) tokens_out: u64,
    pub(crate) frame_uncached_input: u64,
    pub(crate) frame_cache_read: u64,
    pub(crate) frame_cache_write: u64,
    pub(crate) frame_output: u64,
    /// The FINAL wire plan's own input estimate for this attempt: the
    /// conservative floor a zero/under-reported usage frame may never fall
    /// below without evidence (an empty frame is not a free call).
    pub(crate) planned_input_tokens: u64,
    pub(crate) iteration_progress: bool,
    pub(crate) attempt_started: std::time::Instant,
    pub(crate) settled_attempt: u32,
    pub(crate) settled_attempt_identity: Option<ModelCallAttempt>,
    pub(crate) settled_reservation: Option<faktor_session::ReservationId>,
    pub(crate) provider_reported_cost: Option<u64>,
}

/// Control flow of one phase invocation.
pub(crate) enum PhaseFlow {
    /// The drive loop continues with the next phase.
    Continue,
    /// The drive reached its genuine end: return [`TurnDriveState::outcome`].
    End,
}

impl<'a> TurnDriveState<'a> {
    /// Build the drive state from the durable session (the `drive_turn_inner`
    /// preparation block, unchanged): ledger load/heal, task-row restore,
    /// provider + capabilities + context budget, per-turn envelope defaults.
    pub(crate) async fn new(
        rt: &AgentRuntime,
        handle: &'a faktor_session::SessionHandle,
        op_id: OpId,
        cancel: CancellationToken,
        model_override: Option<String>,
    ) -> faktor_core::Result<Self> {
        let outcome = TurnOutcome {
            op_id,
            final_state: AgentState::Preparing,
            turns: 0,
            compacted: false,
            loop_stopped: false,
            stalled: false,
            queued: false,
            verification: Vec::new(),
            acceptance: None,
            review: None,
            completion: None,
            stop_reason: None,
            semantic_risk: None,
            evidence_poll: None,
        };
        // Per-logical-turn accumulation: real steps/failures/files/tests for
        // the durable ledger + memory (audit: only defaults were recorded).
        let turn_summary = faktor_context::ledger::TurnSummary::default();
        let detector = LoopDetector::new(3);
        let mut ledger = rt.load_ledger(handle)?;
        // Retained settled-call outcomes of THIS drive (audit items
        // 13/14/L): the deterministic gate site of the turn re-records each
        // settled call with the explicit verified attribution once the gate
        // verdict exists ("the model said done" alone never learns a
        // success). Bounded by the turn's iteration budget. The paired risk
        // is the semantic risk the turn's route consult attributed to the
        // call (10 = ordinary implement iteration, 70 = stalled-evidence
        // escalation).
        let settled_calls: Vec<(SettledCallOutcome, u8)> = Vec::new();
        let drive_semantic_risk: u8 = 10;
        // The durable task state starts from the user's own goal. The prompt
        // is the authority; the title is only a label and is often generic
        // ("task", "cli run"). A prompt carrying a credential is projected:
        // every secret span is replaced by a fixed marker while the
        // non-secret objective survives. When redaction leaves nothing
        // meaningful the drive refuses typed
        // (`goal_unavailable_after_redaction`) instead of silently adopting
        // the title as the engineering goal.
        initialize_durable_goal(handle, &mut ledger)?;
        // Typed durable ledger (audits 27/71-72): heal the typed entry
        // stream from the legacy blob/task rows when it predates them
        // (goal/criteria first-seen seeding) and record instruction-epoch
        // bumps. Idempotent: the materialized head is refreshed after any
        // append, so nothing re-fires across restarts.
        rt.typed_ledger_drive_start(handle, &ledger)?;
        // Durable Task integration (audit 25): every drive — a fresh turn OR
        // a crash-restarted one — restores the typed task row into the
        // runtime's memory-fact set (task_state + criteria + goal facts,
        // seeded only when absent or stale — never duplicated) and heals the
        // row's spend from durable sources. A missing row is created from
        // the goal so gates always have a row to update.
        rt.restore_task_rows(handle, &mut ledger).await?;
        // Layered lifetimes (audit 26): this drive is ONE turn_budget_ms
        // slice. Wall-clock is measured from the drive's start; expiry is
        // evaluated at every iteration boundary so no single future spans
        // more than one slice (the task re-enters on later turns).
        let slice_started_ms = rt.deps.clock.now_ms();
        // Economic routing (P0-2/85 + attempt-accounting audit D): the
        // per-drive envelope starts from the session-configured side (or
        // the per-message model override); the ROUTING CONSULT itself runs
        // after the drive's first final wire plan exists, on the REAL
        // planned dimensions (input estimate + output cap) — see
        // `route_the_iteration_call` at the model-call site. A routed
        // decision fixes provider/model for the execution of the iteration
        // and becomes the planning model of the next one; there is no
        // fallback: every routing failure is typed and terminal.
        let provider = rt.provider_for(handle)?;
        // allowed-task-id-read: drive-start binding. The identity is captured
        // ONCE here, before any turn work; the whole settlement tail uses this
        // captured id and never re-reads the session's current identity.
        let task_id = handle.task_id()?;
        let routed_decision: Option<RouteDecision> = None;
        let override_active = model_override.is_some();
        let model = match model_override {
            Some(m) => m,
            None => handle.model()?,
        };
        // v7 durable per-turn envelope flag: written once, when the first
        // binding route decision (or the session side, for passthrough)
        // fixes the execution provider/model of this logical turn. Crash
        // recovery resumes from the RECORD, never from whatever the session
        // defaults are afterwards (P1: overrides survive crashes).
        let envelope_fixed = false;
        // The capability view is AUTHORITY-aware: `effective_caps` (the
        // budget/tool/vision decision input, see `resolve_turn_caps`) is the
        // declared profile gated for an Unknown lookup and clamped by the
        // provider's LIVE runtime window (e.g. the Ollama /api/ps allocation,
        // which can sit far below the advertised maximum). Built ONCE per
        // logical turn, so the compaction trigger, try_compact's target, and
        // every post-compaction re-plan all share the SAME effective budget.
        let (caps, cap_authority, effective_caps) = resolve_turn_caps(&provider, &model);
        let effective_caps = apply_context_window_override(handle, effective_caps);
        let budget = ContextBudget::for_capabilities(&effective_caps);
        // P0-79 site d: the hash of the evidence set the last retrieval
        // admitted into this drive's context (drive-local; the tracker's
        // evidence ring is per session and cross-turn).
        let admitted_evidence_hash: Option<u64> = None;
        // One semantic consult per logical turn (audits 54/58): None until a
        // registered provider answers (or forever, when only the fallback is
        // registered — parity).
        let semantic_turn: Option<SemanticTurnState> = None;
        // The LAST typed status of the legacy advisory evidence poll this
        // logical turn (drive-local): None while the index/cold ladder
        // serves, Some once the legacy provider was polled. Re-polling the
        // same turn with the same status dedupes durably (same revision +
        // same bytes).
        let evidence_poll_status: Option<crate::EvidencePollStatus> = None;
        // The structured internal-context channel (`ParentTurnInput`): the
        // already-rendered Faktor section of THIS op, consumed exactly once
        // here, PLUS the DURABLE pending sections the orchestrator's
        // child-terminal settlement recorded for this session (the parent
        // continuation), consumed exactly once here as well. The blocks are
        // DATA rendered under the Faktor-controlled internal-context header
        // — folded into the volatile system slot at every boundary and
        // NEVER conversation messages.
        let mut internal_context = rt.take_internal_context(op_id);
        // Bound the durable pending-handoff block to the system class
        // budget: 200 finished children stay a bounded per-turn section and
        // drain over subsequent turns instead of overflowing the plan.
        let pending = take_pending_internal_context_bounded(handle, budget.system);
        if !pending.is_empty() {
            if !internal_context.is_empty() {
                internal_context.push('\n');
            }
            internal_context.push_str(&pending);
        }
        Ok(Self {
            handle,
            op_id,
            cancel,
            task_id,
            outcome,
            turn_summary,
            detector,
            ledger,
            settled_calls,
            drive_semantic_risk,
            slice_started_ms,
            provider,
            routed_decision,
            override_active,
            model,
            envelope_fixed,
            caps,
            cap_authority,
            effective_caps,
            budget,
            admitted_evidence_hash,
            semantic_turn,
            evidence_poll_status,
            system_extra: String::new(),
            internal_context,
            recent: Vec::new(),
            recent_head_seq: None,
            evidence: Vec::new(),
            project_rules: String::new(),
            repo_map: String::new(),
            history: Vec::new(),
            tool_bundle: None,
            wire_plan: None,
            tool_calls: Vec::new(),
            tool_retry_rounds: 0,
            assistant_message: None,
            text_buf: String::new(),
            reasoning_buf: String::new(),
            tokens_in: 0,
            tokens_out: 0,
            frame_uncached_input: 0,
            frame_cache_read: 0,
            frame_cache_write: 0,
            frame_output: 0,
            planned_input_tokens: 0,
            iteration_progress: false,
            attempt_started: std::time::Instant::now(),
            settled_attempt: 0,
            settled_attempt_identity: None,
            settled_reservation: None,
            provider_reported_cost: None,
        })
    }
}

impl AgentRuntime {
    /// The failure-aware context prior gate (unchanged): the prior applies
    /// only when `failure_learning` is on AND a handle was installed.
    pub(crate) fn turn_context_prior(
        &self,
    ) -> Option<&(dyn faktor_context::information::FailurePrior + Send + Sync)> {
        self.deps
            .efficiency
            .context_prior(self.deps.context_prior.as_deref())
    }

    /// Phase: the iteration boundary (cancel, durable child controls, model
    /// refresh, terminal states, slice expiry, `BuildingContext` hop).
    pub(crate) async fn turn_phase_boundary(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
    ) -> faktor_core::Result<PhaseFlow> {
        if st.cancel.is_cancelled() {
            // Cancel cleanup: the Cancelled classification is the genuine
            // outcome and must not be replaced by an abort error (the
            // abort may race the durable cancel that ended the op), so a
            // lost abort is recorded (marker + audit) and replayed.
            self.dw_note_abort(st.handle, Some(st.op_id), DW_SITE_DRIVE_ABORT_CANCEL);
            st.outcome.final_state = AgentState::Cancelled;
            return Ok(PhaseFlow::End);
        }
        // ---- orchestrated-child steering boundary (audits 20-24)
        // Durable control rows (pause / resume / steer / model / budget)
        // are applied ONLY here — the safe reasoning boundary between
        // operations — never mid-stream and never mid-tool. A pause
        // parks this drive in the durable Waiting phase (a
        // payload-tagged additive state; no new journal EventKind, so
        // protocol golden fixtures stay untouched) until a Resume row or
        // the cancellation token arrives. A model change returns the new
        // selector so the next provider selection uses refreshed
        // capabilities; the current steering note is surfaced into the
        // next wire request below.
        let (model_changed, steer_note) = self
            .drive_boundary_controls(st.handle, st.op_id, &st.cancel, &mut st.model)
            .await?;
        // P2 coordination boundary: the ONE bounded unread notice of an
        // orchestrated child, merged with the durable steering note into
        // the volatile `## Steering` slot. The notice is memoized by the
        // board revision, carries COUNT ONLY (never bodies), and is the
        // sole automatic prompt growth no matter how large the board is.
        let coordination = self.child_coordination_notice(st.handle)?;
        st.system_extra = match coordination {
            Some(notice) if steer_note.is_empty() => notice,
            Some(notice) => format!("{steer_note}\n{notice}"),
            None => steer_note,
        };
        // The Faktor-rendered internal-context block of this turn
        // (`ParentTurnInput` channel) rides the volatile system tail AFTER
        // the steering/coordination slot: it is untrusted DATA from child
        // agents under its own Faktor-controlled `[faktor:internal-context]`
        // header (never instructions, never user authority), and it can
        // never open a prompt section because every child-controlled string
        // was flattened before it entered this string. A turn without an
        // internal block is byte-identical to the pre-channel path.
        if !st.internal_context.is_empty() {
            st.system_extra = if st.system_extra.is_empty() {
                st.internal_context.clone()
            } else {
                format!("{}\n{}", st.system_extra, st.internal_context)
            };
        }
        if model_changed {
            let (caps, cap_authority, effective_caps) = resolve_turn_caps(&st.provider, &st.model);
            let effective_caps = apply_context_window_override(st.handle, effective_caps);
            st.caps = caps;
            st.cap_authority = cap_authority;
            st.effective_caps = effective_caps;
            st.budget = ContextBudget::for_capabilities(&st.effective_caps);
        }
        let state = st.handle.state()?;
        if matches!(
            state,
            AgentState::Cancelled
                | AgentState::Completed
                | AgentState::FailedPermanent
                | AgentState::NeedsUserInput
        ) {
            st.outcome.final_state = state;
            return Ok(PhaseFlow::End);
        }
        // Layered lifetimes (audit 26): no single runtime future may run
        // longer than one turn_budget_ms slice. When the slice expires
        // at an iteration boundary (machine mid-hop at WaitingForModel),
        // the turn ends at its single genuine end — ledger, memory and
        // the task row persist, and the next prompt/queue admission
        // re-enters the task. A stream stuck INSIDE an iteration is a
        // stall problem, bounded by the stall watchdog below.
        if state == AgentState::WaitingForModel
            && self.slice_expired(st.handle, st.slice_started_ms)
        {
            // Legal hop chain from WaitingForModel into the shared
            // genuine-end tail (WaitingForModel -> Streaming is the
            // documented ModelStarted hop).
            st.handle
                .append_journal_event(
                    faktor_core::event::EventKind::ModelStarted,
                    AgentState::Streaming,
                    Some(st.op_id),
                    None,
                )
                .await?;
            self.genuine_end_tail(
                st.handle,
                st.op_id,
                st.task_id,
                &mut st.outcome,
                &mut st.ledger,
                &st.turn_summary,
                &mut st.settled_calls,
                &st.cancel,
            )
            .await?;
            return Ok(PhaseFlow::End);
        }
        // ---- prepare context (fresh turn only; iterations continuing
        // the SAME logical turn after a tool batch arrive with the
        // machine at WaitingForModel and re-plan purely in memory — no
        // journal hops)
        if state != AgentState::WaitingForModel {
            st.handle
                .append_journal_event(
                    faktor_core::event::EventKind::ContextPrepared,
                    AgentState::BuildingContext,
                    Some(st.op_id),
                    None,
                )
                .await?;
        }
        Ok(PhaseFlow::Continue)
    }

    /// Phase: `BuildingContext` — retrieval ladder, semantic/learning DATA,
    /// durable producer archive and the iteration's first wire plan.
    pub(crate) async fn turn_phase_context(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
    ) -> faktor_core::Result<PhaseFlow> {
        // Phase boundary: the drive is preparing context.
        self.note_execution_phase(st.handle, ExecutionPhase::Context);
        let (recent, recent_head_seq) = self.recent_turns_with_head(st.handle, &st.budget).await?;
        // Retrieval signals (spec §20): the CURRENT prompt (the last
        // user turn), the files the task changed, and known failures —
        // never just the session title.
        let prompt = recent
            .iter()
            .rev()
            .find(|t| t.role == "user")
            .map(|t| t.text.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| st.handle.title().unwrap_or_default());
        let evidence_query = EvidenceQuery {
            prompt,
            changed_files: st.ledger.changed_files.clone(),
            failures: st.ledger.known_failures.clone(),
        };
        // Evidence swap (audits 30/64 + P0-30): a READY index
        // generation serves durable index evidence; otherwise the CHEAP
        // cold path (ColdEvidenceProvider) serves the first prompt —
        // a persisted OLD generation when one exists, else targeted
        // reads of the turn's own files. The legacy full bounded scan
        // (deps.evidence) remains ONLY for the case where the
        // IndexService itself cannot be hosted. A first prompt NEVER
        // waits for an index build and NEVER walks the tree.
        // Async turn latency (audit 14/26): the cold ladder is awaited
        // off the turn thread (its supervised git/rg children are the
        // supervisor's, each with its own kill deadline), and the
        // legacy provider is polled on a detached thread under a hard
        // wall budget — a panicking or slow evidence provider degrades
        // to an empty package instead of blocking the turn. A READY
        // index generation whose configured embedder fails is different
        // by contract: the typed provider error surfaces here (no
        // silent lexical-only substitute).
        // ADVISORY POLICY (documented; locked by the
        // `legacy_poll_*_is_durable` tests): a degraded evidence poll
        // NEVER fails the turn by itself. The poll's package feeds the
        // turn exactly as the legacy wrapper did (empty on every
        // degraded status), but the typed status is now a FIRST-CLASS
        // durable fact: archived with the turn producers below under
        // `evidence-poll:<turn_op>` and surfaced in
        // `outcome.evidence_poll` diagnostics. "Retrieval failed" is
        // therefore never silently equivalent to "no evidence found" —
        // a policy that already treats missing evidence as failing can
        // distinguish the two from the durable record.
        let mut evidence_package_meta: Option<faktor_index::EvidencePackageMeta> = None;
        let mut evidence = match self.index_evidence_if_ready(st.handle, &evidence_query)? {
            Some(package) => {
                evidence_package_meta = Some(package.meta);
                package.evidence
            }
            None => match self
                .cold_evidence_if_unready(st.handle, &evidence_query)
                .await
            {
                ColdEvidenceOutcome::Served(evidence, meta) => {
                    evidence_package_meta = Some(meta);
                    evidence
                }
                ColdEvidenceOutcome::Degraded(status) => {
                    // The cold ladder ran but its off-turn bridge failed
                    // (provider panic / unschedulable bridge): the SAME
                    // durable archive + diagnostic path as the legacy
                    // poll, so a broken cold ladder is an EXPLICIT
                    // degradation — never "no evidence", never silent.
                    log_evidence_poll_outcome(&status, LEGACY_EVIDENCE_MAX_WAIT);
                    st.outcome.evidence_poll = Some(encode_evidence_poll_status(&status));
                    st.evidence_poll_status = Some(status);
                    Vec::new()
                }
                ColdEvidenceOutcome::NotHosted => {
                    // Index hosting failed entirely: the legacy bounded
                    // scan is the documented degrade for that case,
                    // polled off the turn thread under a hard wall
                    // deadline (the runtime's verification-path source
                    // probes keep the pooling API out of this file; the
                    // helper lives in lib.rs). The TYPED outcome is kept
                    // (status archived; package unchanged).
                    let crate::EvidencePollOutcome {
                        evidence: polled,
                        status,
                    } = crate::poll_evidence_with_wall_budget_outcome(
                        self.deps.evidence.clone(),
                        st.handle.id(),
                        evidence_query.clone(),
                        LEGACY_EVIDENCE_MAX_WAIT,
                    )
                    .await;
                    log_evidence_poll_outcome(&status, LEGACY_EVIDENCE_MAX_WAIT);
                    st.outcome.evidence_poll = Some(encode_evidence_poll_status(&status));
                    st.evidence_poll_status = Some(status);
                    polled
                }
            },
        };
        if let Some(meta) = &evidence_package_meta {
            // Audit 16: the package's generation/identity/freshness is a
            // first-class, observable fact of the turn's evidence.
            tracing::debug!(
                generation = meta.generation,
                freshness = meta.freshness.as_str(),
                coverage_complete = meta.coverage.complete,
                fingerprint_complete = meta.fingerprint.complete,
                "evidence package freshness"
            );
        }
        // Semantic provider DATA (audits 49/54/58/77): ONE bounded,
        // guarded consult per logical turn, and only when a REGISTERED
        // provider covers the operation. The payload rides as
        // provenance-tagged DATA alongside the retrieved evidence;
        // provider absence, failure or a parked call leaves the turn's
        // decisions byte-identical (parity) while a registered provider
        // that cannot answer degrades to conservative Unknown risk.
        if st.semantic_turn.is_none() {
            st.semantic_turn = semantic_turn_consult(
                &self.deps,
                st.handle,
                &evidence_query.changed_files,
                &st.cancel,
            )
            .await;
        }
        let semantic_evidence: Vec<Evidence> = match &st.semantic_turn {
            Some(state) => {
                st.outcome.semantic_risk = Some(state.level);
                state.evidence.clone()
            }
            None => Vec::new(),
        };
        // Durable learning-corpus DATA (audits 65-69/82): with
        // `failure_learning` on, learnings mined from this session's
        // durable failed/recovered attempts ride the turn's evidence as
        // metadata-only `learning:<digest>` DATA items. The wire planner
        // exposes that digest through the candidate's `omission_keys`,
        // which the installed failure prior resolves against the same
        // durable corpus — the mined loop reaches selection here. Flag
        // off or an empty corpus adds nothing (byte parity); a corrupt
        // row is logged loudly and leaves the turn neutral.
        let learning_evidence = self.learning_corpus_evidence(st.handle);
        // THE durable evidence authority is the store of record (schema
        // v21): the retrieval ladder, the semantic DATA and the learning
        // corpus archive into it as PRODUCERS (cold/index stay
        // producers), and the ContextCompiler selects the turn's
        // evidence from durable state.
        self.archive_turn_producers(
            st.handle,
            st.task_id,
            &evidence,
            &semantic_evidence,
            &learning_evidence,
        );
        // ... and this turn's typed advisory poll status rides the SAME
        // durable authority: a distinct, losslessly-encoded row (never
        // conflated with the producers).
        if let Some(status) = &st.evidence_poll_status {
            self.archive_turn_evidence_poll(st.handle, st.task_id, st.op_id, status);
        }
        // The volatile competition claims of THIS turn (adaptive
        // marginal-information budget): loaded history, produced
        // evidence, semantic/handoff DATA and learning/tool-note DATA.
        // Claims are deterministic bounded estimates; required evidence
        // is hard-reserved inside the compiler regardless of them.
        let volatile = VolatileClaims {
            history_tokens: estimate_recent_tokens(&recent),
            evidence_tokens: estimate_evidence_tokens(&evidence),
            handoff_tokens: estimate_evidence_tokens(&semantic_evidence),
            tool_notes_tokens: estimate_evidence_tokens(&learning_evidence),
        };
        evidence.extend(semantic_evidence);
        evidence.extend(learning_evidence);
        // One selector (audit 33/41/42): when the information-gain flag
        // is on, the compiled selection REPLACES the producer list; a
        // neutral/empty compile or a typed retrieval/overflow error
        // keeps the producers byte-for-byte (never a silent drop, and
        // required content is never destroyed by a fallback).
        match self.compile_turn_evidence(st.handle, st.task_id, &st.ledger, &st.budget, volatile) {
            Ok(Some(selected)) => evidence = selected,
            Ok(None) => {}
            Err(err) => tracing::warn!(
                session = %st.handle.id(),
                "context compiler kept producer evidence: {err}"
            ),
        }
        // Provenance guard (security adversarial suite): retrieved,
        // semantic and learning snippets are repository/provider DATA
        // and may never carry instruction authority into the system
        // prefix. Any snippet containing an override phrase is withheld
        // here (bounded, case/whitespace-insensitive scan) with a warn;
        // the model can still read the file through its tools, so no
        // capability is lost — only the prompt-injection channel.
        evidence.retain(|item| {
            if faktor_security::contains_instruction_override(&item.snippet) {
                tracing::warn!(
                    path = %item.path,
                    "withholding evidence snippet: instruction-override phrasing detected \
                     (repository content is data, not instruction authority)"
                );
                false
            } else {
                true
            }
        });
        // P0-79 site d: a retrieval that ADMITTED a NEW evidence set
        // into the context (non-empty and different from the last set
        // this drive admitted) is semantic progress — the op is
        // consuming new information, not re-reading the same rows.
        if !evidence.is_empty() {
            let set_hash = evidence_set_hash(&evidence);
            if st.admitted_evidence_hash != Some(set_hash) {
                st.admitted_evidence_hash = Some(set_hash);
                self.progress_evidence(st.handle.id(), ProgressEvidence::NewEvidenceAdmitted);
            }
        }
        // Repository knowledge (spec §8 class 3): bounded file map +
        // AGENTS.md rules ride the cacheable prefix. Re-resolved every
        // iteration so edits made by tools appear on the next hop.
        // I-11: the first hop captures this turn's instruction bundle and
        // every later hop is pinned to it (repo_knowledge owns the capture).
        let (project_rules, repo_map) = self.repo_knowledge(st.handle, st.op_id);
        // Audit 61-64: the bounded typed-memory DATA block (V2 rows,
        // newest-first, hard byte budget) rides the semi-stable head
        // right after the repository rules.
        let memory_data = self.memory_data_block(st.handle);
        let project_rules = if memory_data.is_empty() {
            project_rules
        } else if project_rules.is_empty() {
            memory_data
        } else {
            format!("{project_rules}\n{memory_data}")
        };
        let (mut history, _history_head_seq) = self
            .history_messages_with_head(st.handle, &st.budget)
            .await?;
        // Request construction (media): resolve the task's durable image
        // attachments from the CAS into bounded in-memory parts BEFORE
        // the plan is measured, so their token cost is budgeted and a
        // vision-less model refuses here (typed) rather than after a
        // paid call. Never persisted: the durable row keeps ids.
        self.inject_attachment_media(
            st.handle,
            &mut history,
            &st.effective_caps,
            st.provider.as_ref(),
        )?;
        // The wire-plan entry (P0-27): ONE selector. The planner picks
        // the conversation window and the evidence by utility per token
        // over the whole loaded content; plan_wire_turn hands the
        // renderer exactly the planned slices (the renderer's own trim
        // is provably inert on this path). Byte-stable cacheable-prefix
        // semantics are the renderer's and unchanged.
        //
        // Audit 47: tool schemas ride the phase bundle, not the whole
        // registry. This loop drives the Implement phase (the routed
        // call's intent is `implement_main`); the Implement bundle keeps
        // the full registered set, so the wire shape is unchanged while
        // every other phase gets its strict subset.
        //
        // Faktor Acquire (docs/acquire.md §2/§4): the bundle is built
        // under this SESSION's durable lazy-tool activation set, folded
        // with the newest user text by the deterministic detector. With
        // no `/source` flag and no signal the set stays empty and this is
        // byte-identical to `bundle_for_phase` — zero schema tokens for
        // every ordinary prompt.
        let tool_bundle = self.tools_bundle_for_turn(st.handle, &history, &st.effective_caps);
        // Audit 68 production hook: the failure-aware prior is applied
        // ONLY when `failure_learning` is on AND a handle was installed
        // (`None` otherwise — the baseline planner path, byte parity).
        // The prior runs inline on this thread and its panics are NOT
        // caught (a prior must be total); hostile VALUES are clamped by
        // the planner's `prior_adjusted_gain`.
        let wire_plan = plan_wire_turn_with_prior(
            &self.deps.instructions,
            &st.system_extra,
            &tool_bundle.tools,
            &project_rules,
            &st.ledger,
            &repo_map,
            &history,
            &evidence,
            &st.budget,
            &st.model,
            &self.token_cache,
            self.turn_context_prior(),
        )?;
        st.recent = recent;
        st.recent_head_seq = recent_head_seq;
        st.evidence = evidence;
        st.project_rules = project_rules;
        st.repo_map = repo_map;
        st.history = history;
        st.tool_bundle = Some(tool_bundle);
        st.wire_plan = Some(wire_plan);
        Ok(PhaseFlow::Continue)
    }

    /// Phase: planning — proactive compaction + the ONE economic route
    /// consult of the logical turn.
    pub(crate) async fn turn_phase_route(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
    ) -> faktor_core::Result<PhaseFlow> {
        let mut wire_plan = st
            .wire_plan
            .take()
            .expect("context phase planned the wire plan");
        let tool_bundle = st
            .tool_bundle
            .take()
            .expect("context phase bundled the tools");
        // ---- proactive compaction (spec §9)
        let usage = st.budget.effective_usage(wire_plan.total_tokens);
        if usage >= self.deps.compact_at_usage.clamp(0.0, 1.0) {
            // UX-observable in-progress state: child drives expose
            // `execution_phase=compacting` through the durable drive-state
            // projection while the summarizer runs; the end state is the
            // durable ContextCompacted/CompactRejected journal event.
            self.note_execution_phase(st.handle, ExecutionPhase::Compacting);
            let _ = st
                .handle
                .set_runtime_compaction(true, "threshold", self.deps.clock.now_ms());
            let compaction = self
                .try_compact_with_head(
                    st.handle,
                    &st.recent,
                    &st.ledger,
                    &st.budget,
                    &st.cancel,
                    st.recent_head_seq,
                )
                .await;
            let _ = st
                .handle
                .set_runtime_compaction(false, "threshold", self.deps.clock.now_ms());
            self.note_execution_phase(st.handle, ExecutionPhase::Context);
            if let Some(plan) = compaction? {
                st.outcome.compacted = true;
                st.ledger = plan.ledger.clone();
                st.history = recent_turns_to_messages(&plan.kept_recent);
                // Compaction may have dropped the prompt message that
                // carried the turn's images; re-inject so the media
                // survives the re-plan (a synthesized user message is
                // appended when no text-bearing user turn remains).
                self.inject_attachment_media(
                    st.handle,
                    &mut st.history,
                    &st.effective_caps,
                    st.provider.as_ref(),
                )?;
                wire_plan = plan_wire_turn_with_prior(
                    &self.deps.instructions,
                    &st.system_extra,
                    &tool_bundle.tools,
                    &st.project_rules,
                    &st.ledger,
                    &st.repo_map,
                    &st.history,
                    &st.evidence,
                    &st.budget,
                    &st.model,
                    &self.token_cache,
                    self.turn_context_prior(),
                )?;
            }
        }

        // ---- economic route of THIS iteration's model call
        // (attempt-accounting audit D): the consult runs against the
        // FINAL wire plan of the iteration (after compaction
        // replanning) with the REAL planned dimensions — the plan's own
        // input estimate and the execution output cap — never the old
        // hard-coded 16384/2048 guess. Routed ONCE per logical turn:
        // the decision fixes the per-turn execution envelope (interior
        // hops after tool batches stay on the same decision and the
        // same Implement phase). A passthrough decision keeps the
        // session-configured side; EVERY failure is typed and terminal
        // — there is no RouterUnavailable fallback anymore.
        // Phase boundary: planning before the drive's first route.
        if !st.envelope_fixed {
            self.note_execution_phase(st.handle, ExecutionPhase::Planning);
            let mut intent = crate::ModelCallIntent::implement_main();
            // Router qualification (P1 item 10): a turn whose FINAL
            // history carries resolved image parts requires the vision
            // capability, so a vision-less candidate is filtered out by
            // the router BEFORE the provider-boundary refusal and can
            // never be chosen for a call that would be refused anyway.
            // Text-only turns are untouched (byte-identical consult).
            if st
                .history
                .iter()
                .flat_map(|m| m.content.iter())
                .any(|p| matches!(p.kind, faktor_provider::ContentKind::ImageData { .. }))
            {
                intent.required_capabilities.push("vision".into());
            }
            // The wire request carries the planning model's explicit
            // max_output as its output cap (build_request), so price and
            // qualify the route against that same dimension.
            intent.expected_output_tokens = st.effective_caps.max_output.max(1) as u64;
            let dims = crate::wire_plan::planned_request_dimensions(
                &wire_plan,
                st.effective_caps.max_output,
            );
            intent.semantic_risk = if self.progress_stalled_evidence(st.handle.id()) {
                70
            } else {
                10
            };
            // Engineering contract (audit item 4): a high/unknown risk
            // contract raises the routing quality floor and the semantic
            // risk floor observably; a required proof that is still
            // unresolved escalates intelligence/retrieval on this next
            // turn. Empty contracts keep the floors byte-identical.
            let contract_policy = self.contract_policy(st.handle)?;
            let risk_floor = if contract_policy.intelligence_escalation {
                contract_policy.semantic_risk_floor.max(70)
            } else {
                contract_policy.semantic_risk_floor
            };
            intent.semantic_risk = intent.semantic_risk.max(risk_floor);
            if contract_policy.routing_quality_floor > intent.quality.minimum() {
                intent.quality = crate::QualityRequirement::Hard {
                    minimum: contract_policy.routing_quality_floor,
                };
            }
            st.drive_semantic_risk = intent.semantic_risk;
            // Budget view (audit 13): the durable monetary picture is
            // read through the session manager's bounded read pool —
            // the SQLite reads + row decode never run on this Tokio
            // worker. A failed read is NEVER a synthesized unlimited
            // view: a hard-cap (or no-cap-evidence) failure refuses the
            // paid provider call typedly HERE, before routing or any
            // reservation; only a read that PROVED the task explicitly
            // uncapped may proceed — with accounting-unavailable
            // telemetry.
            let view = match self
                .deps
                .session
                .budget_view(st.handle.id(), st.task_id)
                .await
            {
                Ok(view) => Some(view),
                Err(read_err) if read_err.read_failure_is_explicitly_uncapped() => {
                    tracing::warn!(
                        session = %st.handle.id(),
                        "budget accounting unavailable for an explicitly uncapped task: \
                         {read_err}; the st.model call proceeds WITHOUT durable st.budget accounting"
                    );
                    None
                }
                Err(read_err) => {
                    // Fail closed typed: a hard cap that cannot be
                    // enforced means NO paid provider call may be
                    // issued and nothing may be reserved.
                    return Err(read_err.into());
                }
            };
            // RouteRequest semantics: 0 remaining = unlimited.
            let remaining = match &view {
                Some(v) if v.max_cost_micro.is_some() => v.free().min(i64::MAX as u64),
                _ => 0,
            };
            let req = intent.route_request(
                dims.input_estimate_tokens,
                dims.output_cap_tokens,
                remaining,
            );
            // Cache economics consult (P0-82): the session's stored
            // prefix observations ride the consult, so a churning
            // session is priced WITHOUT provider-side cache-read
            // discounts and its decision carries the churn premium. A
            // stability read that fails routes with NO history (no
            // penalty, never an error on the turn — documented). The
            // read runs on the bounded pool (audit 13), never on this
            // Tokio worker.
            let prefix_history = match self
                .deps
                .session
                .provider_prefix_history(st.handle.id())
                .await
            {
                Ok(rows) => Some(rows),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "provider prefix history unreadable; this turn routes without cache history"
                    );
                    None
                }
            }
            .map(|rows| {
                // v19: rows carry the per-call segment observation;
                // the router measures each turn's longest stable
                // leading prefix against its predecessor and consumes
                // it for cache economics. Legacy rows (NULL payload)
                // build the exact pre-v19 TurnPrefix — binary rule.
                faktor_router::stability::TurnPrefix::history_from_persisted(rows.into_iter().map(
                    |r| {
                        (
                            r.row_id as u64,
                            r.prompt_prefix_hash,
                            r.prompt_tokens,
                            r.prefix_segments_json,
                        )
                    },
                ))
            });
            // Candidate-specific sizing (candidate-specific accounting
            // audit): the SAME logical turn is rendered and measured
            // under each seriously-considered candidate's OWN tokenizer
            // through this injected builder; the winning plan crosses
            // back and REPLACES the pre-route render below — the winner
            // is never tokenized or built twice. The pre-route plan is
            // seeded so the planning model itself is never rebuilt.
            let candidate_planner = crate::wire_plan::TurnCandidatePlanner::new(
                &self.deps.instructions,
                &st.system_extra,
                &tool_bundle.tools,
                &st.project_rules,
                &st.ledger,
                &st.repo_map,
                &st.history,
                &st.evidence,
                &st.budget,
                &self.token_cache,
                self.turn_context_prior(),
            );
            candidate_planner.seed(&st.model, wire_plan);
            let mut routed = match self
                .deps
                .routing
                .route_with_session_stability_and_candidate_plans(
                    &req,
                    prefix_history.as_deref(),
                    &candidate_planner,
                ) {
                Ok(d) if d.decision.provider.is_empty() && d.decision.model.is_empty() => {
                    // Documented passthrough (FixedRoutingPolicy test
                    // graph / an unpinned policy): the session-
                    // configured provider/model are the choice.
                    tracing::debug!(session = %st.handle.id(), "routing: passthrough to the session-configured provider/model");
                    None
                }
                Ok(d) => Some(d),
                Err(f) => {
                    // Every routing failure is a TYPED terminal error on
                    // the turn (fail-closed: no silent degradation, no
                    // fallback to a model the router just refused).
                    let message = format!("routing refused the model call: {f:?}");
                    st.outcome.final_state = AgentState::FailedRecoverable;
                    if matches!(f, crate::RouteFailure::BudgetExceeded) {
                        st.outcome.stop_reason = Some(OutcomeReason::new(
                            ReasonCode::BudgetExceeded,
                            message.clone(),
                        ));
                    }
                    // Classified end: the specific gate refusal/outcome
                    // must not be replaced by a journal error, so the
                    // failed Failed-journal is recorded (marker + audit).
                    self.dw_note_journal_failed(
                        st.handle,
                        st.op_id,
                        AgentState::FailedRecoverable,
                        serde_json::json!({ "message": message }),
                        DW_SITE_ROUTING_JOURNAL,
                    )
                    .await;
                    return Ok(PhaseFlow::End);
                }
            };
            let mut winner_plan: Option<faktor_router::CandidatePlan> = None;
            if let Some(sized) = routed.take() {
                let decision = sized.decision;
                winner_plan = sized.plan;
                match self.deps.providers.get(&decision.provider) {
                    Some(p) => st.provider = p,
                    None => {
                        // The router picked a provider the daemon does
                        // not serve: an incoherent graph, never a silent
                        // substitution (fail closed).
                        return Err(Error::new(
                            ErrorKind::Internal,
                            format!(
                                "routing chose provider {:?} which is not registered",
                                decision.provider
                            ),
                        ));
                    }
                }
                st.model = if st.override_active {
                    // A per-message override pins the MODEL; the routed
                    // decision still resolved the execution provider. An
                    // adapter alias (Ollama `default`) is resolved to the
                    // CONCRETE wire id so every durable record names what
                    // was actually called.
                    st.provider.resolved_model(&st.model)
                } else {
                    let routed_model = st.provider.resolved_model(&decision.model);
                    if routed_model != st.model {
                        // Economy may choose among SERVED candidates, but
                        // never silently: the session surface names both
                        // sides. UNSERVED models are refused typed at
                        // session creation (native_create_session), so
                        // this substitution is a priced equivalence, not
                        // a lost model.
                        tracing::warn!(
                            session = %st.handle.id(),
                            requested = %st.model,
                            chosen = %routed_model,
                            "economy routing chose a different served model"
                        );
                    }
                    routed_model
                };
                tracing::info!(session = %st.handle.id(), "routing: {reasoning}", reasoning = decision.reasoning);
                // Typed ledger: the routing DECISION is durable history.
                st.handle.ledger_routing_decision(
                    st.op_id.raw(),
                    &decision.provider,
                    &st.model,
                    &truncate(&decision.reasoning, 4096),
                    decision.estimated_cost_micro,
                )?;
                st.routed_decision = Some(decision);
            }
            // REUSE the winning candidate's measured plan; an unsized
            // policy (pinned/passthrough/legacy) recovers the seeded
            // pre-route plan. Exactly one of the two always exists.
            wire_plan = match winner_plan
                .and_then(|plan| {
                    plan.wire_plan
                        .downcast::<WirePlan>()
                        .ok()
                        .map(|boxed| *boxed)
                })
                .or_else(|| candidate_planner.take_seed_plan())
            {
                Some(plan) => plan,
                None => {
                    return Err(Error::new(
                        ErrorKind::Internal,
                        "candidate sizing lost the winning wire plan".to_string(),
                    ));
                }
            };
            // The execution envelope may differ from the planning
            // model: refresh caps and the context budget for the NEXT
            // iteration's plan (this iteration's plan already fits the
            // routed window — the router's fit axis qualified it).
            let (caps, cap_authority, effective_caps) = resolve_turn_caps(&st.provider, &st.model);
            let effective_caps = apply_context_window_override(st.handle, effective_caps);
            st.caps = caps;
            st.cap_authority = cap_authority;
            st.effective_caps = effective_caps;
            st.budget = ContextBudget::for_capabilities(&st.effective_caps);
            // v7 durable per-turn envelope: written once, at the bind —
            // the moment the logical turn actually drives with its
            // effective provider/model (per-message override wins),
            // reasoning variant and tool mode fixed on the turn record.
            let provider_id = st.handle.provider()?;
            self.guarded_set_turn_envelope(
                st.handle,
                st.op_id,
                &provider_id,
                &st.model,
                None,
                Some(tool_mode_tag(self.deps.tool_call_mode)),
                DW_SITE_TURN_ENVELOPE,
            )?;
            st.envelope_fixed = true;
        }
        st.wire_plan = Some(wire_plan);
        st.tool_bundle = Some(tool_bundle);
        Ok(PhaseFlow::Continue)
    }

    /// Phase: `ExecutingTool` — run the batch and either continue the same
    /// logical turn or land the loop-detection/classified end; the genuine
    /// end tail closes every other path.
    pub(crate) async fn turn_phase_tools_and_end(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
    ) -> faktor_core::Result<PhaseFlow> {
        if !st.tool_calls.is_empty() {
            // Phase boundary: mutating (DiskWrite) batches are Coding;
            // every other batch is Tool. The classification comes from
            // the tool registry's resource class — never from a name
            // string match.
            let batch_phase = if st.tool_calls.iter().any(|(_, name, _)| {
                self.deps
                    .tools
                    .get(name)
                    .map(|t| t.resource_class == faktor_core::resource::ResourceClass::DiskWrite)
                    .unwrap_or(false)
            }) {
                ExecutionPhase::Coding
            } else {
                ExecutionPhase::Tool
            };
            self.note_execution_phase(st.handle, batch_phase);
            let attempted = std::mem::take(&mut st.tool_calls);
            let attempted_count = attempted.len();
            let retryable_mutation = attempted.iter().any(|(_, name, _)| {
                self.deps
                    .tools
                    .get(name)
                    .map(|tool| {
                        tool.resource_class == faktor_core::resource::ResourceClass::DiskWrite
                    })
                    .unwrap_or(false)
            });
            let executed = self
                .run_tool_calls(
                    st.handle,
                    st.tool_bundle
                        .as_ref()
                        .expect("context phase bundled the tools"),
                    st.op_id,
                    &mut st.detector,
                    &mut st.ledger,
                    &mut st.turn_summary,
                    &st.cancel,
                    st.semantic_turn.as_ref(),
                    attempted,
                )
                .await?;
            // Durable cross-turn loop detection (spec §28): the same
            // failing calls repeated across logical turns trip here even
            // though each turn's LoopDetector starts fresh.
            let durable_trip =
                self.durable_loop_signals(st.handle, &st.turn_summary, &st.detector)?;
            // State-based loop fingerprints (P0-78): patch→revert→patch
            // and repeated evidence sets trip EVEN when tools executed
            // (execution success is not progress when the state
            // oscillates) — the typed code rides the outcome.
            let fingerprint_trip = st.detector.last_trip_code().is_some();
            if (executed == 0 && st.detector.trips > 0) || durable_trip || fingerprint_trip {
                // Repeating failing calls / oscillating state: stop and
                // re-plan.
                st.outcome.loop_stopped = true;
                let (code, detail) = match st.detector.last_trip_code() {
                        Some(code) => (
                            code,
                            match code {
                                ReasonCode::PatchRevertPatch => {
                                    "loop detected: repo state oscillating patch/revert/patch with no other change"
                                }
                                ReasonCode::RepeatedEvidenceSet => {
                                    "loop detected: different commands returning the identical evidence set"
                                }
                                other => {
                                    tracing::warn!("unexpected fingerprint trip code {other:?}");
                                    "loop detected: state-level fingerprint"
                                }
                            },
                        ),
                        None => (
                            ReasonCode::LoopDetected,
                            "loop detected: repeated failing tool calls",
                        ),
                    };
                if st.detector.last_trip_code().is_some() {
                    st.outcome.stop_reason = Some(OutcomeReason::new(code, detail.to_string()));
                }
                // Classified end: the loop verdict must not be masked by a
                // journal error — record the lost Failed-journal.
                self.dw_note_journal_failed(
                    st.handle,
                    st.op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({ "message": detail }),
                    DW_SITE_LOOP_JOURNAL,
                )
                .await;
                // Typed ledger (audit 27): this genuine decision point —
                // stop-and-replan — is durable history.
                self.guarded_ledger_decision(
                    st.handle,
                    "replan",
                    "stop the turn and re-plan",
                    detail,
                    DW_SITE_LOOP_DECISION,
                )?;
                st.outcome.final_state = AgentState::FailedRecoverable;
                return Ok(PhaseFlow::End);
            }
            if executed > 0 {
                // Tools ran: the SAME logical turn continues. Interior
                // hops (no TurnCompleted — that is reserved for the one
                // genuine end) return the machine to WaitingForModel so
                // the model can see the tool results. The hop depends on
                // what the batch left behind: all-completed leaves the
                // machine at `Validating`; a MIXED batch (>=1 completed,
                // >=1 failed recoverably) leaves it at `FailedRecoverable`
                // because every failed finish takes that edge. The
                // recovery hop below is the explicit retry/re-plan path
                // — see [`AgentRuntime::walk_tool_batch_to_waiting`] for
                // the state diagram.
                self.walk_tool_batch_to_waiting(st.handle, st.op_id).await?;
                // Stream again with tool results (machine at WaitingForModel):
                // the loop continues with the next phase pass.
                return Ok(PhaseFlow::Continue);
            }
            if st.handle.state()? == AgentState::FailedRecoverable {
                // A recoverable mismatch in a MUTATION batch (stale
                // expected_hash, malformed operation, missing file) is not
                // task completion: the model can see the typed tool error
                // and correct the arguments. The retry re-enters through
                // the legal FailedRecoverable -> Preparing hop; the loop
                // detector still ends a repeated identical failure, and the
                // bound keeps this finite. Other all-failed batches (denied
                // permission, unknown tool, shell failure) keep the
                // classified end below.
                const MAX_MUTATION_RETRIES: u8 = 2;
                if retryable_mutation
                    && attempted_count > 0
                    && st.tool_retry_rounds < MAX_MUTATION_RETRIES
                {
                    st.tool_retry_rounds += 1;
                    // The batch's own `put_task_ledger` just overwrote the
                    // blob with the ledger it was handed, so the decision
                    // must ride THIS ledger object (the typed entry alone is
                    // not enough for the next prompt).
                    st.ledger
                        .decisions
                        .push("retry the failed edit with corrected arguments".to_string());
                    self.guarded_ledger_decision(
                        st.handle,
                        "retry",
                        "retry the failed edit with corrected arguments",
                        "a mutating tool call failed recoverably; the typed error is in the transcript",
                        DW_SITE_LOOP_DECISION,
                    )?;
                    self.walk_tool_batch_to_waiting(st.handle, st.op_id).await?;
                    return Ok(PhaseFlow::Continue);
                }
                // executed == 0 with at least one submitted tool that
                // failed recoverably: the turn's classified end. The
                // machine is already at `FailedRecoverable` (the failed
                // finishes), so this is an honest report, not a
                // transition; the durable per-tool rows carry the
                // outcomes. `FailedRecoverable -> Validating` is ILLEGAL
                // by design — the old fall-through into the genuine-end
                // tail died there with `InvalidState`.
                st.outcome.final_state = AgentState::FailedRecoverable;
                return Ok(PhaseFlow::End);
            }
            // executed == 0: every call was denied or unknown. If the
            // loop detector tripped we returned above; otherwise the
            // turn genuinely ends below (the denials already moved the
            // machine toward ReadyForNextTurn).
        }

        // ---- genuine end of the logical turn: validate → update
        // memory → ONE TurnCompleted → ReadyForNextTurn.
        self.genuine_end_tail(
            st.handle,
            st.op_id,
            st.task_id,
            &mut st.outcome,
            &mut st.ledger,
            &st.turn_summary,
            &mut st.settled_calls,
            &st.cancel,
        )
        .await?;
        Ok(PhaseFlow::End)
    }
}
