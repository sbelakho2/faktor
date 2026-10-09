//! `runtime::turn::drive`: cohesive slice of the turn module.

use super::*;

impl AgentRuntime {
    /// Submit a prompt and run the full turn (durable; survives restarts).
    pub async fn run_turn(
        self: &Arc<Self>,
        session: SessionId,
        prompt: &str,
        files: &[String],
    ) -> faktor_core::Result<TurnOutcome> {
        self.run_turn_with_model(session, prompt, files, None).await
    }

    /// Run the parent's next logical turn from a BOUNDED typed child handoff
    /// (audit: typed child handoff consumption). The child's transcript
    /// never enters the request: only [`ChildHandoff::render_bounded`] does,
    /// and it is guaranteed to fit `budget_tokens` before the prompt is
    /// submitted. Everything omitted stays retrievable through the
    /// handoff's scoped refs.
    pub async fn run_turn_from_handoff(
        self: &Arc<Self>,
        session: SessionId,
        handoff: &ChildHandoff,
        budget_tokens: usize,
    ) -> faktor_core::Result<TurnOutcome> {
        let prompt = handoff.render_bounded(budget_tokens);
        self.run_turn(session, &prompt, &[]).await
    }

    /// Like [`AgentRuntime::run_turn`] with a per-message model override.
    /// When `Some`, the override model is used for provider capability
    /// lookup and request building INSTEAD of the session's configured
    /// model; the provider is always the session's provider, and a model
    /// the provider has no capabilities for falls back to the provider's
    /// default capabilities (never an error at send time). The journaled
    /// session row keeps its original model — the override is per-message,
    /// not a session mutation.
    pub async fn run_turn_with_model(
        self: &Arc<Self>,
        session: SessionId,
        prompt: &str,
        files: &[String],
        model: Option<String>,
    ) -> faktor_core::Result<TurnOutcome> {
        let receipt = self.submit(session, prompt, files)?;
        if receipt.queued {
            // A single per-session turn runner delivers queued prompts after
            // the active logical turn completes (audit round 6). Never start
            // a second concurrent turn.
            return Ok(TurnOutcome {
                op_id: receipt.op_id,
                final_state: AgentState::Idle,
                turns: 0,
                compacted: false,
                loop_stopped: false,
                stalled: false,
                queued: true,
                verification: Vec::new(),
                acceptance: None,
                review: None,
                completion: None,
                stop_reason: None,
                semantic_risk: None,
                evidence_poll: None,
            });
        }
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        self.drive_receipt(&handle, receipt, model).await
    }

    /// Synchronous prompt submission (journal + durable queue when busy).
    /// The server uses this to answer with the TRUE queued state before
    /// spawning any detached work (audit round 6).
    pub fn submit(
        self: &Arc<Self>,
        session: SessionId,
        prompt: &str,
        files: &[String],
    ) -> faktor_core::Result<faktor_session::PromptReceipt> {
        self.submit_with_op_id(session, prompt, files, None)
    }

    /// [`Self::submit`] under a PREALLOCATED operation id (audit P1): the
    /// durable admission claim reserved the id BEFORE the prompt was
    /// accepted, so the claim's `reservation` names exactly the turn this
    /// submit journals and recovery can rebuild the receipt from the
    /// durable turn/queue facts. `None` mints a fresh id as before.
    pub fn submit_with_op_id(
        self: &Arc<Self>,
        session: SessionId,
        prompt: &str,
        files: &[String],
        reserved_op_id: Option<OpId>,
    ) -> faktor_core::Result<faktor_session::PromptReceipt> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        // Crash recovery first (never blindly re-run).
        self.recover_session(&handle)?;
        handle.submit_prompt_with_op_id(prompt, files, reserved_op_id)
    }

    /// Persist a child drive's coarse execution phase at a safe boundary
    /// (additive projection). Best-effort by design: the phase never gates
    /// lifecycle, budgeting or verification — a failed write is logged and
    /// the drive proceeds. Non-orchestrated sessions are a typed no-op
    /// inside [`faktor_session::SessionHandle::set_execution_phase`].
    pub(crate) fn note_execution_phase(
        &self,
        handle: &faktor_session::SessionHandle,
        phase: ExecutionPhase,
    ) {
        if let Err(e) = handle.set_execution_phase(phase) {
            tracing::debug!(
                session = %handle.id(),
                phase = phase.as_str(),
                error = %e.message,
                "execution phase persist failed"
            );
        }
    }

    /// The durable-control steering boundary of an orchestrated child
    /// (audits 20-24): reads the child's own control queue and applies every
    /// pending message in seq order — pause parks the drive (Waiting phase,
    /// non-interrupting), resume/steer/change-model/change-budget take
    /// effect here, and cancel terminates the turn through the existing
    /// bounded abort path. Applied rows are acked exactly once (idempotent
    /// store ack; a crash between effect and ack re-applies the same
    /// idempotent effect). Returns `(model_changed, current_steering_note)`:
    /// the note is durable state the caller surfaces at the next provider
    /// selection. Non-child sessions return immediately.
    pub(crate) async fn drive_boundary_controls(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        cancel: &CancellationToken,
        model: &mut String,
    ) -> faktor_core::Result<(bool, String)> {
        use faktor_session::child::{ChildControl, ChildPhase};

        // Fast path: sessions without a single memory fact carry no
        // orchestration rows (one bounded probe query).
        let probe = handle.memory_facts_page(None, 1)?;
        if probe.total_estimate == 0 {
            return Ok((false, String::new()));
        }
        if handle.orchestrator_child_identity_get()?.is_none() {
            return Ok((false, String::new())); // not an orchestrated child
        }
        // Crash-resume re-application: a ChangeModel applied (and acked)
        // before a crash must still steer this re-attached drive.
        let (mut changed_model, mut note) = {
            let ds = handle.orchestrator_drive_state_get()?;
            if !ds.current_model.is_empty() && *model != ds.current_model {
                *model = ds.current_model.clone();
            }
            (false, ds.current_note.clone())
        };
        loop {
            let pending = handle.orchestrator_ctl_pending()?;
            if pending.is_empty() {
                return Ok((changed_model, note));
            }
            let mut parked = false;
            for row in pending {
                let seq = row.seq;
                match &row.control {
                    ChildControl::Pause => {
                        // Effect first, ack second: a crash in between
                        // re-applies the same idempotent phase write.
                        let mut ds = handle.orchestrator_drive_state_get()?;
                        ds.phase = ChildPhase::Waiting;
                        ds.updated_ms = handle.now_ms();
                        handle.orchestrator_drive_state_put(&ds)?;
                        handle.orchestrator_ctl_ack(seq)?;
                        parked = true;
                    }
                    ChildControl::Resume => {
                        let mut ds = handle.orchestrator_drive_state_get()?;
                        ds.phase = ChildPhase::Running;
                        ds.updated_ms = handle.now_ms();
                        handle.orchestrator_drive_state_put(&ds)?;
                        handle.orchestrator_ctl_ack(seq)?;
                    }
                    ChildControl::Steer { note: next } => {
                        // Runtime-boundary guard (the HTTP layer is never the
                        // only guard): a whitespace-only note is malformed
                        // and must never be applied or acked.
                        if next.trim().is_empty() {
                            return Err(faktor_core::Error::malformed(
                                "queued steering note must not be empty or whitespace-only",
                            ));
                        }
                        // The note is durable state, not a one-shot event:
                        // every later iteration re-reads the drive state and
                        // the caller surfaces it at the next provider
                        // selection.
                        let mut ds = handle.orchestrator_drive_state_get()?;
                        ds.current_note = next.clone();
                        ds.updated_ms = handle.now_ms();
                        handle.orchestrator_drive_state_put(&ds)?;
                        handle.orchestrator_ctl_ack(seq)?;
                        note = next.clone();
                    }
                    ChildControl::ChangeModel { model: next } => {
                        let mut ds = handle.orchestrator_drive_state_get()?;
                        ds.current_model = next.clone();
                        ds.updated_ms = handle.now_ms();
                        handle.orchestrator_drive_state_put(&ds)?;
                        handle.orchestrator_ctl_ack(seq)?;
                        *model = next.clone();
                        changed_model = true;
                    }
                    ChildControl::ChangeBudget { max_tokens } => {
                        // ACK == durable: the idempotent Task-row patch is
                        // applied FIRST and a failed patch propagates WITHOUT
                        // acking, so the control stays pending and a retry
                        // re-applies the same patch. A returned error always
                        // means "not applied", never a silent divergence.
                        self.seed_task_budget(
                            handle.id(),
                            &faktor_session::TaskBudget {
                                max_tokens: Some(*max_tokens),
                                max_turns: None,
                                spent_tokens: 0,
                                spent_turns: 0,
                            },
                        )?;
                        handle.orchestrator_ctl_ack(seq)?;
                    }
                    ChildControl::Cancel => {
                        // The executor normally fires the cancellation token;
                        // when this boundary observes the durable Cancel
                        // first (restart race), terminate the active turn
                        // through the SAME bounded abort path — never leave a
                        // dead or half-cancelled session. Effect FIRST, ack
                        // second (ACK == durable): a failed abort propagates
                        // WITHOUT acking, so the pending row is retried
                        // instead of leaving an applied control whose effect
                        // never happened.
                        if !cancel.is_cancelled() {
                            handle.abort(Some(op_id))?;
                        }
                        handle.orchestrator_ctl_ack(seq)?;
                    }
                    ChildControl::Retry => {
                        // Retry is executor-applied (re-drive from durable
                        // records happens between drives, never inside one).
                        // Acked here only when a stale row races a live drive
                        // (the executor acks before re-driving, so a pending
                        // Retry alongside an active drive is dead state).
                        handle.orchestrator_ctl_ack(seq)?;
                    }
                }
            }
            if parked {
                // Park at the safe boundary: the session row stays mid-turn
                // (Waiting is a durable payload-tagged child state, not a new
                // EventKind — protocol golden fixtures are untouched). The
                // bounded sleep re-scans the durable queue; the cancellation
                // token ends the park immediately. A crash here is recovered
                // by the executor's normal re-attach (the drive re-enters and
                // re-parks until a Resume row exists). The park itself is
                // BOUNDED by the configured turn budget (fallback when the
                // operator opted out with 0): a child whose Resume never
                // arrives returns a TYPED timeout so the executor re-drives
                // it — it is never polled forever.
                let park_deadline = bounded_turn_wait(handle, MAX_CHILD_PARK_WAIT);
                let park_started = Instant::now();
                loop {
                    if cancel.is_cancelled() {
                        return Ok((changed_model, note));
                    }
                    let ds = handle.orchestrator_drive_state_get()?;
                    if ds.phase != ChildPhase::Waiting {
                        break; // resumed by a concurrent path: re-scan rows
                    }
                    let pending = handle.orchestrator_ctl_pending()?;
                    let resume = pending
                        .iter()
                        .any(|r| matches!(r.control, ChildControl::Resume));
                    let cancel_row = pending
                        .iter()
                        .any(|r| matches!(r.control, ChildControl::Cancel));
                    if cancel_row {
                        // Durable Cancel with no token (executor died): the
                        // bounded abort path cancels the active turn; the
                        // session stays promptable. Effect first, ack second:
                        // a failed abort propagates WITHOUT acking, so the
                        // pending Cancel row is retried.
                        handle.abort(Some(op_id))?;
                        for r in pending {
                            if matches!(r.control, ChildControl::Cancel) {
                                handle.orchestrator_ctl_ack(r.seq)?;
                            }
                        }
                        return Ok((changed_model, note));
                    }
                    if resume {
                        break; // the outer scan applies the Resume row
                    }
                    let parked_for = park_started.elapsed();
                    if parked_for >= park_deadline {
                        return Err(faktor_core::Error::timeout(format!(
                            "child {} stayed parked (Waiting) for {park_deadline:?} without a Resume \
                             or Cancel control (parked {parked_for:?}); the executor must re-drive \
                             the child at this boundary",
                            handle.id()
                        )));
                    }
                    tokio::time::sleep(Duration::from_millis(150).min(park_deadline - parked_for))
                        .await;
                }
                // Re-scan from the queue head: rows enqueued while parked are
                // applied at this same boundary, in seq order.
                continue;
            }
        }
    }

    /// Drive an already-submitted turn receipt to its single genuine end.
    /// A failed turn journals FailedRecoverable (never stuck mid-transition)
    /// and lands the session in a promptable state.
    pub async fn drive_receipt(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        receipt: faktor_session::PromptReceipt,
        model: Option<String>,
    ) -> faktor_core::Result<TurnOutcome> {
        let op_id = receipt.op_id;
        let cancel = receipt.op_meta.cancellation.clone();
        let outcome = self.drive_turn(handle, op_id, cancel, model).await;
        if let Err(e) = &outcome {
            // Cleanup path: the ORIGINAL turn error must reach the caller, so
            // a failed Failed-journal is recorded (marker + audit) instead.
            // A failure while parked at WaitingForPermission (a permission
            // timeout) has NO legal edge to FailedRecoverable: forcing it
            // wrote a false crash marker. Blocked is the legal "no live
            // owner" landing; every other failure keeps FailedRecoverable.
            let target = match handle.state() {
                // `WaitingForPermission` has no edge to FailedRecoverable;
                // its legal honest landing is ReadyForNextTurn (the timeout
                // is reported by the event, and the session stays usable).
                Ok(AgentState::WaitingForPermission) => AgentState::ReadyForNextTurn,
                _ => AgentState::FailedRecoverable,
            };
            self.dw_note_journal_failed(
                handle,
                op_id,
                target,
                serde_json::json!({ "message": e.message }),
                DW_SITE_RECEIPT_JOURNAL,
            )
            .await;
            // The interrupted logical turn cannot resume: close its record
            // so no later recovery tries to continue a dead turn.
            self.dw_note_finish_turn_record(handle, op_id, "failed", DW_SITE_RECEIPT_RECORD);
        }
        outcome
    }

    /// Continue a turn interrupted by a crash: load the interrupted logical
    /// turn's durable record (v7), verify the session state, resolve side
    /// effects (tool-run recovery incl. exactly-once idempotent replay), and
    /// resume the state machine driving the SAME recorded turn op id with
    /// the SAME recorded provider/model envelope — never a synthesized
    /// operation and never the session's current defaults.
    pub async fn continue_turn(
        self: &Arc<Self>,
        session: SessionId,
    ) -> faktor_core::Result<TurnOutcome> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        let Some(record) = handle.active_turn_record()? else {
            return Err(Error::conflict(format!(
                "session {session} has no interrupted logical turn to continue"
            )));
        };
        self.continue_record(&handle, &record).await
    }

    /// Drive an admitted queued prompt as a logical turn. Same
    /// failure-finalization semantics as immediate turns (drive_receipt):
    /// an error journals FailedRecoverable so the session is never stranded.
    pub(crate) async fn drive_admitted(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        admitted: &faktor_session::AdmittedQueuedPrompt,
    ) -> faktor_core::Result<TurnOutcome> {
        let token = handle.turn_cancellation(admitted.op_id).unwrap_or_default();
        let model = admitted.model.clone();
        let outcome = self.drive_turn(handle, admitted.op_id, token, model).await;
        if let Err(e) = &outcome {
            // Cleanup path: the ORIGINAL turn error must reach the caller, so
            // a failed Failed-journal is recorded (marker + audit) instead.
            self.dw_note_journal_failed(
                handle,
                admitted.op_id,
                AgentState::FailedRecoverable,
                serde_json::json!({ "message": e.message }),
                DW_SITE_ADMITTED_JOURNAL,
            )
            .await;
            self.dw_note_finish_turn_record(
                handle,
                admitted.op_id,
                "failed",
                DW_SITE_ADMITTED_RECORD,
            );
        }
        outcome
    }

    /// Interior state hop back to WaitingForModel after crash recovery using
    /// ONLY legal machine transitions (never a blind re-entry).
    pub(crate) fn walk_to_waiting(
        &self,
        handle: &faktor_session::SessionHandle,
        op: OpId,
    ) -> faktor_core::Result<()> {
        match handle.state()? {
            AgentState::Validating => {
                handle.append_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    AgentState::UpdatingMemory,
                    Some(op),
                    None,
                )?;
                handle.append_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    AgentState::WaitingForModel,
                    Some(op),
                    None,
                )?;
            }
            AgentState::UpdatingMemory | AgentState::Streaming => {
                handle.append_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    AgentState::WaitingForModel,
                    Some(op),
                    None,
                )?;
            }
            AgentState::ExecutingTool if handle.pending_tool_runs()?.is_empty() => {
                // The audited permission window: the drive parked on the
                // durable permission and died BEFORE starting any tool run.
                // Resolving it with `Allow` lands the machine on ExecutingTool
                // with nothing to execute; the SAME recorded turn must be
                // re-planned, so hop back through the documented internal
                // chain (ExecutingTool -> Validating -> UpdatingMemory ->
                // WaitingForModel) instead of failing on an illegal
                // ExecutingTool -> BuildingContext transition. A turn WITH
                // pending tool runs already replayed them above.
                for target in [
                    AgentState::Validating,
                    AgentState::UpdatingMemory,
                    AgentState::WaitingForModel,
                ] {
                    handle.append_event(
                        faktor_core::event::EventKind::PhaseChanged,
                        target,
                        Some(op),
                        None,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ------------------------------------------------------------ the turn loop

    /// Drive ONE logical turn to its single genuine end. Queued-prompt
    /// isolation (audit round 6) happens in the history loader: user
    /// messages of undelivered queued prompts never enter this turn's
    /// context.
    /// Create (once) the assistant message row a stream writes parts onto.
    /// Message seq follows the durable journal (proposed = newest + 1); the
    /// append itself runs through the manager's DbActor (audit 42).
    pub(crate) async fn ensure_assistant_message(
        &self,
        handle: &faktor_session::SessionHandle,
        mid: &mut Option<i64>,
    ) -> faktor_core::Result<i64> {
        if let Some(m) = *mid {
            return Ok(m);
        }
        let seq = handle.proposed_message_seq()?;
        let m = handle
            .append_message(seq, "assistant", serde_json::json!({ "parts": [] }))
            .await?;
        *mid = Some(m);
        Ok(m)
    }

    pub(crate) async fn drive_turn(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        cancel: CancellationToken,
        model_override: Option<String>,
    ) -> faktor_core::Result<TurnOutcome> {
        let session = handle.id();
        // Progress record: this logical turn is the in-flight operation.
        self.progress_begin_op(session, op_id);
        let mut outcome = self
            .drive_turn_inner(handle, op_id, cancel, model_override)
            .await;
        // Machine stop reasons (audit 94): the failing paths inside the
        // drive journal prose; the coded reason is derived here from the
        // outcome so every stall/loop/cancel stop carries (code, detail).
        if let Ok(o) = &mut outcome {
            if o.stop_reason.is_none() {
                o.stop_reason = match o.final_state {
                    AgentState::Cancelled => Some(OutcomeReason::new(
                        ReasonCode::Cancelled,
                        "the turn was cancelled",
                    )),
                    _ if o.stalled => Some(OutcomeReason::new(
                        ReasonCode::Stalled,
                        "stall detected: no output, progress or completed op within the silence budget",
                    )),
                    _ if o.loop_stopped => Some(OutcomeReason::new(
                        ReasonCode::LoopDetected,
                        "loop detected: repeated failing tool calls across the batch",
                    )),
                    _ => None,
                };
            }
        }
        // The durable turn record follows the machine: a genuine end closes
        // the record so recovery never continues a finished turn.
        if let Ok(o) = &outcome {
            let status = match o.final_state {
                AgentState::ReadyForNextTurn | AgentState::Completed => "completed",
                AgentState::Cancelled => "cancelled",
                _ => "failed",
            };
            // Success path: the outcome is the report; a failed close must
            // not be reported as a failed turn, so it is recorded instead.
            self.dw_note_finish_turn_record(handle, op_id, status, DW_SITE_DRIVE_RECORD);
        }
        // Op done: completion is progress evidence; the record (with its
        // last_op_completed_at) stays observable until the session ends.
        self.progress_end_op(session);
        outcome
    }

    pub(crate) async fn drive_turn_inner(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        cancel: CancellationToken,
        model_override: Option<String>,
    ) -> faktor_core::Result<TurnOutcome> {
        let mut outcome = TurnOutcome {
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
        let mut turn_summary = faktor_context::ledger::TurnSummary::default();
        let mut detector = LoopDetector::new(3);
        let mut ledger = self.load_ledger(handle)?;
        // Retained settled-call outcomes of THIS drive (audit items
        // 13/14/L): the deterministic gate site of the turn re-records each
        // settled call with the explicit verified attribution once the gate
        // verdict exists ("the model said done" alone never learns a
        // success). Bounded by the turn's iteration budget. The paired risk
        // is the semantic risk the turn's route consult attributed to the
        // call (10 = ordinary implement iteration, 70 = stalled-evidence
        // escalation).
        let mut settled_calls: Vec<(SettledCallOutcome, u8)> = Vec::new();
        let mut drive_semantic_risk: u8 = 10;
        // The durable task state starts from the user's own goal: the first
        // prompt (session title) — audit round: goal was never set.
        if ledger.goal.is_empty() {
            ledger.goal = truncate(&handle.title()?, 200).to_string();
        }
        // Typed durable ledger (audits 27/71-72): heal the typed entry
        // stream from the legacy blob/task rows when it predates them
        // (goal/criteria first-seen seeding) and record instruction-epoch
        // bumps. Idempotent: the materialized head is refreshed after any
        // append, so nothing re-fires across restarts.
        self.typed_ledger_drive_start(handle, &ledger)?;
        // Durable Task integration (audit 25): every drive — a fresh turn OR
        // a crash-restarted one — restores the typed task row into the
        // runtime's memory-fact set (task_state + criteria + goal facts,
        // seeded only when absent or stale — never duplicated) and heals the
        // row's spend from durable sources. A missing row is created from
        // the goal so gates always have a row to update.
        self.restore_task_rows(handle, &ledger)?;
        // Layered lifetimes (audit 26): this drive is ONE turn_budget_ms
        // slice. Wall-clock is measured from the drive's start; expiry is
        // evaluated at every iteration boundary so no single future spans
        // more than one slice (the task re-enters on later turns).
        let slice_started_ms = self.deps.clock.now_ms();
        // Economic routing (P0-2/85 + attempt-accounting audit D): the
        // per-drive envelope starts from the session-configured side (or
        // the per-message model override); the ROUTING CONSULT itself runs
        // after the drive's first final wire plan exists, on the REAL
        // planned dimensions (input estimate + output cap) — see
        // `route_the_iteration_call` at the model-call site. A routed
        // decision fixes provider/model for the execution of the iteration
        // and becomes the planning model of the next one; there is no
        // fallback: every routing failure is typed and terminal.
        let mut provider = self.provider_for(handle)?;
        let task_id = handle.task_id()?;
        let mut routed_decision: Option<RouteDecision> = None;
        let override_active = model_override.is_some();
        let mut model = match model_override {
            Some(m) => m,
            None => handle.model()?,
        };
        // v7 durable per-turn envelope flag: written once, when the first
        // binding route decision (or the session side, for passthrough)
        // fixes the execution provider/model of this logical turn. Crash
        // recovery resumes from the RECORD, never from whatever the session
        // defaults are afterwards (P1: overrides survive crashes).
        let mut envelope_fixed = false;
        let mut caps = provider.capabilities(&model);
        // P0 (runtime context override): a provider's LIVE runtime window
        // (e.g. the Ollama /api/ps allocation, which can sit far below the
        // advertised 256K model maximum) is the real budget ceiling. When
        // the provider reports one, budget from min(model max, runtime
        // limit); None means no live data and the model maximum stands
        // (today's behavior — safe direction). Built ONCE per logical turn,
        // so the compaction trigger, try_compact's target, and every
        // post-compaction re-plan all share the SAME effective budget.
        let mut effective_caps = caps.clone();
        if let Some(limit) = provider.runtime_context_limit(&model) {
            effective_caps.context = effective_caps.context.min(limit);
        }
        let mut budget = ContextBudget::for_capabilities(&effective_caps);
        // P0-79 site d: the hash of the evidence set the last retrieval
        // admitted into this drive's context (drive-local; the tracker's
        // evidence ring is per session and cross-turn).
        let mut admitted_evidence_hash: Option<u64> = None;
        // One semantic consult per logical turn (audits 54/58): None until a
        // registered provider answers (or forever, when only the fallback is
        // registered — parity).
        let mut semantic_turn: Option<SemanticTurnState> = None;
        // The LAST typed status of the legacy advisory evidence poll this
        // logical turn (drive-local): None while the index/cold ladder
        // serves, Some once the legacy provider was polled. Re-polling the
        // same turn with the same status dedupes durably (same revision +
        // same bytes).
        let mut evidence_poll_status: Option<crate::EvidencePollStatus> = None;
        loop {
            if cancel.is_cancelled() {
                // Cancel cleanup: the Cancelled classification is the genuine
                // outcome and must not be replaced by an abort error (the
                // abort may race the durable cancel that ended the op), so a
                // lost abort is recorded (marker + audit) and replayed.
                self.dw_note_abort(handle, Some(op_id), DW_SITE_DRIVE_ABORT_CANCEL);
                outcome.final_state = AgentState::Cancelled;
                return Ok(outcome);
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
                .drive_boundary_controls(handle, op_id, &cancel, &mut model)
                .await?;
            // P2 coordination boundary: the ONE bounded unread notice of an
            // orchestrated child, merged with the durable steering note into
            // the volatile `## Steering` slot. The notice is memoized by the
            // board revision, carries COUNT ONLY (never bodies), and is the
            // sole automatic prompt growth no matter how large the board is.
            let coordination = self.child_coordination_notice(handle)?;
            let system_extra = match coordination {
                Some(notice) if steer_note.is_empty() => notice,
                Some(notice) => format!("{steer_note}\n{notice}"),
                None => steer_note,
            };
            if model_changed {
                caps = provider.capabilities(&model);
                effective_caps = caps.clone();
                if let Some(limit) = provider.runtime_context_limit(&model) {
                    effective_caps.context = effective_caps.context.min(limit);
                }
                budget = ContextBudget::for_capabilities(&effective_caps);
            }
            let state = handle.state()?;
            if matches!(
                state,
                AgentState::Cancelled
                    | AgentState::Completed
                    | AgentState::FailedPermanent
                    | AgentState::NeedsUserInput
            ) {
                outcome.final_state = state;
                return Ok(outcome);
            }
            // Layered lifetimes (audit 26): no single runtime future may run
            // longer than one turn_budget_ms slice. When the slice expires
            // at an iteration boundary (machine mid-hop at WaitingForModel),
            // the turn ends at its single genuine end — ledger, memory and
            // the task row persist, and the next prompt/queue admission
            // re-enters the task. A stream stuck INSIDE an iteration is a
            // stall problem, bounded by the stall watchdog below.
            if state == AgentState::WaitingForModel && self.slice_expired(handle, slice_started_ms)
            {
                // Legal hop chain from WaitingForModel into the shared
                // genuine-end tail (WaitingForModel -> Streaming is the
                // documented ModelStarted hop).
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::ModelStarted,
                        AgentState::Streaming,
                        Some(op_id),
                        None,
                    )
                    .await?;
                self.genuine_end_tail(
                    handle,
                    op_id,
                    &mut outcome,
                    &mut ledger,
                    &turn_summary,
                    &mut settled_calls,
                    &cancel,
                )
                .await?;
                return Ok(outcome);
            }
            // ---- prepare context (fresh turn only; iterations continuing
            // the SAME logical turn after a tool batch arrive with the
            // machine at WaitingForModel and re-plan purely in memory — no
            // journal hops)
            if state != AgentState::WaitingForModel {
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::ContextPrepared,
                        AgentState::BuildingContext,
                        Some(op_id),
                        None,
                    )
                    .await?;
            }
            // Phase boundary: the drive is preparing context.
            self.note_execution_phase(handle, ExecutionPhase::Context);
            let recent = self.recent_turns(handle, &budget).await?;
            // Retrieval signals (spec §20): the CURRENT prompt (the last
            // user turn), the files the task changed, and known failures —
            // never just the session title.
            let prompt = recent
                .iter()
                .rev()
                .find(|t| t.role == "user")
                .map(|t| t.text.clone())
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| handle.title().unwrap_or_default());
            let evidence_query = EvidenceQuery {
                prompt,
                changed_files: ledger.changed_files.clone(),
                failures: ledger.known_failures.clone(),
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
            let mut evidence = match self.index_evidence_if_ready(handle, &evidence_query)? {
                Some(package) => {
                    evidence_package_meta = Some(package.meta);
                    package.evidence
                }
                None => match self.cold_evidence_if_unready(handle, &evidence_query).await {
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
                        outcome.evidence_poll = Some(encode_evidence_poll_status(&status));
                        evidence_poll_status = Some(status);
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
                            handle.id(),
                            evidence_query.clone(),
                            LEGACY_EVIDENCE_MAX_WAIT,
                        )
                        .await;
                        log_evidence_poll_outcome(&status, LEGACY_EVIDENCE_MAX_WAIT);
                        outcome.evidence_poll = Some(encode_evidence_poll_status(&status));
                        evidence_poll_status = Some(status);
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
            if semantic_turn.is_none() {
                semantic_turn = semantic_turn_consult(
                    &self.deps,
                    handle,
                    &evidence_query.changed_files,
                    &cancel,
                )
                .await;
            }
            let semantic_evidence: Vec<Evidence> = match &semantic_turn {
                Some(state) => {
                    outcome.semantic_risk = Some(state.level);
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
            let learning_evidence = self.learning_corpus_evidence(handle);
            // THE durable evidence authority is the store of record (schema
            // v21): the retrieval ladder, the semantic DATA and the learning
            // corpus archive into it as PRODUCERS (cold/index stay
            // producers), and the ContextCompiler selects the turn's
            // evidence from durable state.
            self.archive_turn_producers(
                handle,
                task_id,
                &evidence,
                &semantic_evidence,
                &learning_evidence,
            );
            // ... and this turn's typed advisory poll status rides the SAME
            // durable authority: a distinct, losslessly-encoded row (never
            // conflated with the producers).
            if let Some(status) = &evidence_poll_status {
                self.archive_turn_evidence_poll(handle, task_id, op_id, status);
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
            match self.compile_turn_evidence(handle, task_id, &ledger, &budget, volatile) {
                Ok(Some(selected)) => evidence = selected,
                Ok(None) => {}
                Err(err) => tracing::warn!(
                    session = %handle.id(),
                    "context compiler kept producer evidence: {err}"
                ),
            }
            // P0-79 site d: a retrieval that ADMITTED a NEW evidence set
            // into the context (non-empty and different from the last set
            // this drive admitted) is semantic progress — the op is
            // consuming new information, not re-reading the same rows.
            if !evidence.is_empty() {
                let set_hash = evidence_set_hash(&evidence);
                if admitted_evidence_hash != Some(set_hash) {
                    admitted_evidence_hash = Some(set_hash);
                    self.progress_evidence(handle.id(), ProgressEvidence::NewEvidenceAdmitted);
                }
            }
            // Repository knowledge (spec §8 class 3): bounded file map +
            // AGENTS.md rules ride the cacheable prefix. Re-resolved every
            // iteration so edits made by tools appear on the next hop.
            let (project_rules, repo_map) = self.repo_knowledge(handle);
            // Audit 61-64: the bounded typed-memory DATA block (V2 rows,
            // newest-first, hard byte budget) rides the semi-stable head
            // right after the repository rules.
            let memory_data = self.memory_data_block(handle);
            let project_rules = if memory_data.is_empty() {
                project_rules
            } else if project_rules.is_empty() {
                memory_data
            } else {
                format!("{project_rules}\n{memory_data}")
            };
            let mut history = self.history_messages(handle, &budget).await?;
            // Request construction (media): resolve the task's durable image
            // attachments from the CAS into bounded in-memory parts BEFORE
            // the plan is measured, so their token cost is budgeted and a
            // vision-less model refuses here (typed) rather than after a
            // paid call. Never persisted: the durable row keeps ids.
            self.inject_attachment_media(handle, &mut history, &effective_caps, provider.as_ref())?;
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
            let tool_bundle = self.tools_bundle_for_turn(handle, &history, &effective_caps);
            // Audit 68 production hook: the failure-aware prior is applied
            // ONLY when `failure_learning` is on AND a handle was installed
            // (`None` otherwise — the baseline planner path, byte parity).
            // The prior runs inline on this thread and its panics are NOT
            // caught (a prior must be total); hostile VALUES are clamped by
            // the planner's `prior_adjusted_gain`.
            let context_prior = self
                .deps
                .efficiency
                .context_prior(self.deps.context_prior.as_deref());
            let mut wire_plan = plan_wire_turn_with_prior(
                &self.deps.instructions,
                &system_extra,
                &tool_bundle.tools,
                &project_rules,
                &ledger,
                &repo_map,
                &history,
                &evidence,
                &budget,
                &model,
                &self.token_cache,
                context_prior,
            )?;

            // ---- proactive compaction (spec §9)
            let usage = budget.effective_usage(wire_plan.total_tokens);
            if usage >= self.deps.compact_at_usage.clamp(0.0, 1.0) {
                if let Some(plan) = self
                    .try_compact(handle, &recent, &ledger, &budget, &cancel)
                    .await?
                {
                    outcome.compacted = true;
                    ledger = plan.ledger.clone();
                    history = recent_turns_to_messages(&plan.kept_recent);
                    // Compaction may have dropped the prompt message that
                    // carried the turn's images; re-inject so the media
                    // survives the re-plan (a synthesized user message is
                    // appended when no text-bearing user turn remains).
                    self.inject_attachment_media(
                        handle,
                        &mut history,
                        &effective_caps,
                        provider.as_ref(),
                    )?;
                    wire_plan = plan_wire_turn_with_prior(
                        &self.deps.instructions,
                        &system_extra,
                        &tool_bundle.tools,
                        &project_rules,
                        &ledger,
                        &repo_map,
                        &history,
                        &evidence,
                        &budget,
                        &model,
                        &self.token_cache,
                        context_prior,
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
            if !envelope_fixed {
                self.note_execution_phase(handle, ExecutionPhase::Planning);
                let mut intent = crate::ModelCallIntent::implement_main();
                // Router qualification (P1 item 10): a turn whose FINAL
                // history carries resolved image parts requires the vision
                // capability, so a vision-less candidate is filtered out by
                // the router BEFORE the provider-boundary refusal and can
                // never be chosen for a call that would be refused anyway.
                // Text-only turns are untouched (byte-identical consult).
                if history
                    .iter()
                    .flat_map(|m| m.content.iter())
                    .any(|p| matches!(p.kind, faktor_provider::ContentKind::ImageData { .. }))
                {
                    intent.required_capabilities.push("vision".into());
                }
                // The wire request carries no explicit max_output
                // (build_request), so the provider's own output bound is
                // the call's real output cap: price and qualify the route
                // against the planning model's cap.
                intent.expected_output_tokens = effective_caps.max_output.max(1) as u64;
                let dims = crate::wire_plan::planned_request_dimensions(
                    &wire_plan,
                    effective_caps.max_output,
                );
                intent.semantic_risk = if self.progress_stalled_evidence(handle.id()) {
                    70
                } else {
                    10
                };
                drive_semantic_risk = intent.semantic_risk;
                // Budget view (audit 13): the durable monetary picture is
                // read through the session manager's bounded read pool —
                // the SQLite reads + row decode never run on this Tokio
                // worker. A failed read is NEVER a synthesized unlimited
                // view: a hard-cap (or no-cap-evidence) failure refuses the
                // paid provider call typedly HERE, before routing or any
                // reservation; only a read that PROVED the task explicitly
                // uncapped may proceed — with accounting-unavailable
                // telemetry.
                let view = match self.deps.session.budget_view(handle.id(), task_id).await {
                    Ok(view) => Some(view),
                    Err(read_err) if read_err.read_failure_is_explicitly_uncapped() => {
                        tracing::warn!(
                            session = %handle.id(),
                            "budget accounting unavailable for an explicitly uncapped task: \
                             {read_err}; the model call proceeds WITHOUT durable budget accounting"
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
                let prefix_history = self
                    .deps
                    .session
                    .provider_prefix_history(handle.id())
                    .await
                    .ok()
                    .map(|rows| {
                        // v19: rows carry the per-call segment observation;
                        // the router measures each turn's longest stable
                        // leading prefix against its predecessor and consumes
                        // it for cache economics. Legacy rows (NULL payload)
                        // build the exact pre-v19 TurnPrefix — binary rule.
                        faktor_router::stability::TurnPrefix::history_from_persisted(
                            rows.into_iter().map(|r| {
                                (
                                    r.row_id as u64,
                                    r.prompt_prefix_hash,
                                    r.prompt_tokens,
                                    r.prefix_segments_json,
                                )
                            }),
                        )
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
                    &system_extra,
                    &tool_bundle.tools,
                    &project_rules,
                    &ledger,
                    &repo_map,
                    &history,
                    &evidence,
                    &budget,
                    &self.token_cache,
                    context_prior,
                );
                candidate_planner.seed(&model, wire_plan);
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
                        tracing::debug!(session = %handle.id(), "routing: passthrough to the session-configured provider/model");
                        None
                    }
                    Ok(d) => Some(d),
                    Err(f) => {
                        // Every routing failure is a TYPED terminal error on
                        // the turn (fail-closed: no silent degradation, no
                        // fallback to a model the router just refused).
                        let message = format!("routing refused the model call: {f:?}");
                        outcome.final_state = AgentState::FailedRecoverable;
                        if matches!(f, crate::RouteFailure::BudgetExceeded) {
                            outcome.stop_reason = Some(OutcomeReason::new(
                                ReasonCode::BudgetExceeded,
                                message.clone(),
                            ));
                        }
                        // Classified end: the specific gate refusal/outcome
                        // must not be replaced by a journal error, so the
                        // failed Failed-journal is recorded (marker + audit).
                        self.dw_note_journal_failed(
                            handle,
                            op_id,
                            AgentState::FailedRecoverable,
                            serde_json::json!({ "message": message }),
                            DW_SITE_ROUTING_JOURNAL,
                        )
                        .await;
                        return Ok(outcome);
                    }
                };
                let mut winner_plan: Option<faktor_router::CandidatePlan> = None;
                if let Some(sized) = routed.take() {
                    let decision = sized.decision;
                    winner_plan = sized.plan;
                    match self.deps.providers.get(&decision.provider) {
                        Some(p) => provider = p,
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
                    model = if override_active {
                        // A per-message override pins the MODEL; the routed
                        // decision still resolved the execution provider.
                        model
                    } else {
                        let routed_model = decision.model.clone();
                        if routed_model != model {
                            // Economy may choose among SERVED candidates, but
                            // never silently: the session surface names both
                            // sides. UNSERVED models are refused typed at
                            // session creation (native_create_session), so
                            // this substitution is a priced equivalence, not
                            // a lost model.
                            tracing::warn!(
                                session = %handle.id(),
                                requested = %model,
                                chosen = %routed_model,
                                "economy routing chose a different served model"
                            );
                        }
                        routed_model
                    };
                    tracing::info!(session = %handle.id(), "routing: {reasoning}", reasoning = decision.reasoning);
                    // Typed ledger: the routing DECISION is durable history.
                    handle.ledger_routing_decision(
                        op_id.raw(),
                        &decision.provider,
                        &model,
                        &truncate(&decision.reasoning, 4096),
                        decision.estimated_cost_micro,
                    )?;
                    routed_decision = Some(decision);
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
                caps = provider.capabilities(&model);
                effective_caps = caps.clone();
                if let Some(limit) = provider.runtime_context_limit(&model) {
                    effective_caps.context = effective_caps.context.min(limit);
                }
                budget = ContextBudget::for_capabilities(&effective_caps);
                // v7 durable per-turn envelope: written once, at the bind —
                // the moment the logical turn actually drives with its
                // effective provider/model (per-message override wins),
                // reasoning variant and tool mode fixed on the turn record.
                let provider_id = handle.provider()?;
                self.guarded_set_turn_envelope(
                    handle,
                    op_id,
                    &provider_id,
                    &model,
                    None,
                    Some(tool_mode_tag(self.deps.tool_call_mode)),
                    DW_SITE_TURN_ENVELOPE,
                )?;
                envelope_fixed = true;
            }

            // ---- provider call (state-aware retry, spec §13): a request
            // that failed BEFORE any content became durable may retry under
            // the retry policy (network class, bounded backoff). Once a tool
            // ran or assistant content was flushed, never replay.
            // Phase boundary: the drive is reasoning (paid provider call).
            self.note_execution_phase(handle, ExecutionPhase::Reasoning);
            handle
                .append_journal_event(
                    faktor_core::event::EventKind::ModelStarted,
                    AgentState::WaitingForModel,
                    Some(op_id),
                    None,
                )
                .await?;
            // Attempt-accounting audit: the policy is read ONCE per logical
            // call (an override installed mid-flight never changes the retry
            // shape of an in-flight turn).
            let retry_policy = self.effective_retry_policy();
            let max_attempts = retry_policy.max_attempts.max(1);
            let mut iteration_progress = false;
            let mut assistant_message: Option<i64> = None;
            let mut text_buf = String::new();
            let mut reasoning_buf = String::new();
            let mut tool_calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut tokens_in = 0u64;
            let mut tokens_out = 0u64;
            // P0-1 settlement truth: the LAST usage frame's token categories
            // (uncached input / cache reads / cache writes / output —
            // reasoning billed at the output line) ride the settlement call,
            // so each reported category is priced at its OWN frozen
            // route-time line, never at a fabricated aggregate.
            let mut frame_uncached_input = 0u64;
            let mut frame_cache_read = 0u64;
            let mut frame_cache_write = 0u64;
            let mut frame_output = 0u64;
            // Telemetry basis of the logical call (P0-28): the final
            // attempt's latency + attempt count are recorded at the
            // terminal outcome sites after the loop.
            let mut attempt_started = std::time::Instant::now();
            let mut settled_attempt = 0u32;
            // The FINAL attempt's identity + reservation (attempt-accounting
            // audit): the attempt loop breaks with the settling attempt, so
            // its keyed row is written at the iteration's completed site
            // AFTER the loop from these captured values.
            let mut settled_attempt_identity: Option<ModelCallAttempt> = None;
            let mut settled_reservation: Option<faktor_session::ReservationId> = None;
            use futures::StreamExt;
            'attempts: for attempt in 0..max_attempts {
                if attempt > 0 {
                    // Bounded exponential backoff with jitter before the
                    // next try (spec §13).
                    let delay = retry_policy.next_delay(attempt - 1);
                    tokio::time::sleep(delay).await;
                }
                // Telemetry latency basis of this (final) attempt.
                attempt_started = std::time::Instant::now();
                settled_attempt = attempt;
                // Attempt identity (attempt-accounting audit): EVERY
                // physical retry is a NEW durable attempt with a fresh op
                // id and its OWN reservation; earlier uncertain attempts
                // keep their own reservations (they consume the parent task
                // budget until reconciled/finalized). Each attempt's
                // ATTEMPT-KEYED provider-call row is written ONCE at its
                // terminal site below (failed / completed) with this
                // identity.
                let attempt_identity =
                    ModelCallAttempt::new(op_id, self.deps.session.try_next_op_id()?, attempt)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Internal,
                                "the attempt op id collided with the logical op id",
                            )
                        })?;
                settled_attempt_identity = Some(attempt_identity);
                let request =
                    self.build_request(handle, &wire_plan, op_id, &model, &cancel, attempt)?;
                CapabilityValidator::validate(&request, &caps)?;
                // The vision-like DOCUMENT gate at the wire boundary: a
                // provider that does not carry document parts can never
                // receive a resolved FileData part (defense in depth
                // behind the injection gate).
                CapabilityValidator::validate_documents(
                    &request,
                    provider.document_capable(&model),
                )?;
                handle
                    .append_journal_event(
                        faktor_core::event::EventKind::ModelStarted,
                        AgentState::Streaming,
                        Some(op_id),
                        None,
                    )
                    .await?;

                // Durable budget gate (P0-6/12): reserve BEFORE reaching the
                // provider. The prediction is the conservative payload
                // estimate — chars/3 as a token-count proxy for calls no
                // pricing authority priced — or, when the routing decision
                // priced the call, at least the decision's microUSD
                // estimate. The prediction is ONLY a free-budget ceiling:
                // the settlement later records the ACTUAL cost (reported or
                // categories x the frozen snapshot) and releases the
                // difference — never a fabricated tokens x 1 microUSD
                // actual.
                let mut provider_reported_cost: Option<u64> = None;
                let est = (request.system.len() as u64 / 3)
                    .saturating_add(
                        request
                            .messages
                            .iter()
                            .map(|m| {
                                m.content
                                    .iter()
                                    .map(|p| match &p.kind {
                                        faktor_provider::ContentKind::Text { text }
                                        | faktor_provider::ContentKind::Reasoning { text } => {
                                            text.len() as u64
                                        }
                                        _ => 0,
                                    })
                                    .sum::<u64>()
                            })
                            .sum::<u64>()
                            / 3,
                    )
                    .saturating_add(256);
                let predicted = match &routed_decision {
                    Some(d) if d.estimated_cost_micro > 0 => est.max(d.estimated_cost_micro),
                    _ => est,
                };
                let route_json = routed_decision
                    .as_ref()
                    .and_then(|d| serde_json::to_string(d).ok());
                let reservation = match self
                    .deps
                    .budgets
                    .reserve_attempt(
                        handle.id(),
                        task_id,
                        attempt_identity,
                        predicted,
                        // P0-1: the reserve freezes the route decision's
                        // price capture on the reservation row (None = no
                        // pricing authority — the unpriced pin/passthrough
                        // paths; settlement then refuses under a hard cap
                        // instead of fabricating an actual).
                        routed_decision
                            .as_ref()
                            .and_then(|d| d.pricing_snapshot.clone()),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(SessionBudgetError::BudgetExceeded { .. }) => {
                        outcome.final_state = AgentState::FailedRecoverable;
                        let message = format!(
                            "budget exceeded: request would cost {predicted} micro of the remaining task budget"
                        );
                        outcome.stop_reason = Some(OutcomeReason::new(
                            ReasonCode::BudgetExceeded,
                            message.clone(),
                        ));
                        // Classified end: the budget refusal must not be
                        // masked by a journal error — record the lost append.
                        self.dw_note_journal_failed(
                            handle,
                            op_id,
                            AgentState::FailedRecoverable,
                            serde_json::json!({ "message": message }),
                            DW_SITE_BUDGET_JOURNAL,
                        )
                        .await;
                        return Ok(outcome);
                    }
                    Err(e) => return Err(e.into()),
                };
                settled_reservation = Some(reservation);
                // The attempt's budget machine (attempt-accounting audit):
                // every terminal call of this attempt goes through the
                // guarded machine — refund only pre-dispatch, UNCERTAIN for
                // any post-dispatch failure, settle exactly once.
                let mut acct = crate::AttemptAccounting::new(
                    self.deps.budgets.clone(),
                    handle.id(),
                    Some(reservation),
                );
                // The additive commercial debit (Wave 5 residual): a
                // Faktor-managed attempt opens a durable credit hold BEFORE
                // anything leaves the process (record-before-call); a BYOK
                // model, or a daemon without an installed authority, takes
                // the documented no-op path and is byte-identical. A refused
                // or unavailable debit is a pre-dispatch refusal: the budget
                // reservation is released and the provider is never called.
                let mut debits = match self.attempt_debits(
                    handle.id(),
                    task_id,
                    provider.id(),
                    &model,
                    attempt_identity,
                    predicted,
                ) {
                    Ok(machine) => machine,
                    Err(e) => {
                        if let Err(refund_err) = acct.fail_before_dispatch().await {
                            tracing::error!(
                                session = %handle.id(),
                                "refund of reservation {reservation} after a malformed debit identity failed: {refund_err}"
                            );
                        }
                        outcome.final_state = AgentState::FailedRecoverable;
                        let message = format!("managed credit debit refused before dispatch: {e}");
                        outcome.stop_reason.clone_from(&Some(OutcomeReason::new(
                            ReasonCode::BudgetExceeded,
                            message.clone(),
                        )));
                        // Classified end: record the lost Failed-journal
                        // instead of masking the debit refusal with an error.
                        self.dw_note_journal_failed(
                            handle,
                            op_id,
                            AgentState::FailedRecoverable,
                            serde_json::json!({ "message": message }),
                            DW_SITE_DEBIT_IDENTITY_JOURNAL,
                        )
                        .await;
                        return Ok(outcome);
                    }
                };
                if let Err(e) = debits.begin() {
                    tracing::warn!(
                        session = %handle.id(),
                        attempt = %attempt_identity.attempt_op_id,
                        "managed credit debit refused before dispatch: {e}"
                    );
                    // The provider was provably never contacted: release the
                    // budget reservation (pre-dispatch refund) and stop.
                    if let Err(refund_err) = acct.fail_before_dispatch().await {
                        tracing::error!(
                            session = %handle.id(),
                            "refund of reservation {reservation} after a refused debit failed: {refund_err}"
                        );
                    }
                    outcome.final_state = AgentState::FailedRecoverable;
                    let message = format!("managed credit debit refused before dispatch: {e}");
                    outcome.stop_reason = Some(OutcomeReason::new(
                        ReasonCode::BudgetExceeded,
                        message.clone(),
                    ));
                    // Classified end: record the lost Failed-journal instead
                    // of masking the refusal with a journal error.
                    self.dw_note_journal_failed(
                        handle,
                        op_id,
                        AgentState::FailedRecoverable,
                        serde_json::json!({ "message": message }),
                        DW_SITE_DEBIT_REFUSED_JOURNAL,
                    )
                    .await;
                    return Ok(outcome);
                }
                // P0-2: the durable dispatch marker is written immediately
                // BEFORE the provider request is sent. Crash recovery splits
                // surviving OPEN rows on it: never-dispatched -> REFUNDED,
                // may-have-been-billed -> UNCERTAIN (which keeps consuming
                // the reserved amount until a reconcile or the task-end
                // finalize). A stream that cannot be durably marked must not
                // start: sending an unmarked request would recreate the
                // $0-crash-charge hole this marker closes.
                if let Err(e) = acct.mark_dispatched().await {
                    tracing::error!(
                        session = %handle.id(),
                        "cannot mark reservation {reservation} dispatched: {e}"
                    );
                    // The marker write failed BEFORE the request was sent:
                    // the provider was provably never contacted — release
                    // the reserved row (definitely-not-sent), then fail.
                    if let Err(refund_err) = acct.fail_before_dispatch().await {
                        tracing::error!(
                            session = %handle.id(),
                            "refund of the never-dispatched reservation {reservation} failed: {refund_err}"
                        );
                    }
                    // Same pre-dispatch truth for the credit hold: the
                    // provider was never called, so the hold is refunded.
                    if let Err(refund_err) = debits.refund("dispatch_marker_failed") {
                        tracing::error!(
                            session = %handle.id(),
                            "refund of the pre-dispatch credit hold failed: {refund_err}"
                        );
                    }
                    return Err(e.into());
                }
                // The provider request is about to leave the process: from
                // here the credit hold may only settle or stay uncertain.
                debits.mark_dispatched();
                let mut stream = provider.stream(request);
                // Stall watchdog (spec §28, stall vs progress): while the
                // stream is awaited, a bounded tick evaluates the session's
                // progress record. Total silence — no output chunks AND no
                // progress/heartbeat evidence AND no completed op — past the
                // silence budget means the provider call is stuck: stop it
                // like any honest stream failure (never silently wait on a
                // dead stream; a long-running op that emits periodic chunks
                // or progress updates can never trip this).
                let mut stall_ticks =
                    tokio::time::interval(std::time::Duration::from_millis(STALL_POLL_MS));
                loop {
                    tokio::select! {
                        biased;
                        chunk = stream.next() => {
                            let Some(chunk) = chunk else { break };
                            if cancel.is_cancelled() {
                                // Cancel-AFTER-dispatch (attempt accounting):
                                // the request left the process and the
                                // provider MAY have billed — the machine
                                // marks the attempt UNCERTAIN (a refund
                                // would be the $0-charge hole; the reserved
                                // amount keeps consuming until reconcile or
                                // the task-end finalize).
                                if let Err(uncertain_err) = acct
                                    .fail_after_dispatch("cancelled_after_dispatch", None)
                                    .await
                                {
                                    tracing::warn!(
                                        session = %handle.id(),
                                        "cannot mark the cancelled attempt uncertain: {uncertain_err}"
                                    );
                                }
                                // The credit hold stays durable: the request
                                // left the process and the provider may have
                                // billed (settlement/reconciliation later).
                                debits.close_uncertain();
                                // Flush the partial stream: the buffered text
                                // and reasoning were emitted live but never
                                // journaled, so a cancel used to leave an
                                // empty assistant message.
                                if !text_buf.is_empty() || !reasoning_buf.is_empty() {
                                    let mid = self
                                        .ensure_assistant_message(handle, &mut assistant_message)
                                        .await?;
                                    if !text_buf.is_empty() {
                                        handle.append_text_part(mid, &text_buf).await?;
                                        text_buf.clear();
                                    }
                                    if !reasoning_buf.is_empty() {
                                        handle.append_reasoning_part(mid, &reasoning_buf).await?;
                                        reasoning_buf.clear();
                                    }
                                }
                                // Cancel cleanup: recorded (marker + audit),
                                // never a different error for the Cancelled end.
                                self.dw_note_abort(handle, Some(op_id), DW_SITE_DRIVE_ABORT_DISPATCH);
                                outcome.final_state = AgentState::Cancelled;
                                return Ok(outcome);
                            }
                            match chunk {
                                Ok(ProviderChunk::Text { text }) => {
                                    text_buf.push_str(&text);
                                    let mid = self.ensure_assistant_message(handle, &mut assistant_message).await?;
                                    self.emit_chunk(handle.id(), Some(mid), "text", &text);
                                    iteration_progress = true;
                                    // EPHEMERAL path: text deltas are NOT journaled per
                                    // chunk (a multi-hour agent would commit millions of
                                    // tiny SQLite events). The durable representation is
                                    // the message part, flushed in bounded segments so a
                                    // crash loses at most one segment.
                                    if text_buf.len() >= STREAM_FLUSH_BYTES {
                                        handle.append_text_part(mid, &text_buf).await?;
                                        text_buf.clear();
                                    }
                                }
                                Ok(ProviderChunk::Reasoning { text }) => {
                                    reasoning_buf.push_str(&text);
                                    let mid = self.ensure_assistant_message(handle, &mut assistant_message).await?;
                                    self.emit_chunk(handle.id(), Some(mid), "reasoning", &text);
                                    iteration_progress = true;
                                    if reasoning_buf.len() >= STREAM_FLUSH_BYTES {
                                        handle.append_reasoning_part(mid, &reasoning_buf).await?;
                                        reasoning_buf.clear();
                                    }
                                }
                                Ok(ProviderChunk::ToolCall {
                                    id,
                                    name,
                                    input,
                                    complete,
                                }) => {
                                    if !complete {
                                        return Err(Error::malformed(format!(
                                            "incomplete tool call {id} without completion"
                                        )));
                                    }
                                    let mid = self.ensure_assistant_message(handle, &mut assistant_message).await?;
                                    handle.append_tool_call_part(
                                        mid,
                                        &id,
                                        &name,
                                        input.clone(),
                                        "completed",
                                    ).await?;
                                    self.emit_chunk(
                                        handle.id(),
                                        Some(mid),
                                        "tool",
                                        &format!(
                                            "{name}\n{}",
                                            serde_json::to_string(&input).unwrap_or_default()
                                        ),
                                    );
                                    tool_calls.push((id, name, input));
                                }
                                Ok(ProviderChunk::Usage(usage)) => {
                                    // Canonical usage (audit Phase-1 item C):
                                    // the adapter already split the wire's
                                    // cache detail off the uncached input
                                    // counter at its own boundary, so the
                                    // frame's categories price directly at
                                    // the reservation's frozen quote lines.
                                    // Never re-derive provider semantics and
                                    // never add cache lines on top of an
                                    // uncached counter that still contains
                                    // them — that ambiguity is what
                                    // double-billed cache reads.
                                    let recorded_input = recorded_input_total(&usage);
                                    tokens_in = recorded_input;
                                    tokens_out = usage.output_tokens;
                                    // The LAST usage frame wins (providers
                                    // settle once, usually at the end): its
                                    // categories are the settlement basis
                                    // (uncached input + the cache lines +
                                    // output — reasoning already folds into
                                    // the output line at the adapter, so
                                    // the informational reasoning subset is
                                    // never billed a second time).
                                    frame_uncached_input = usage.uncached_input_tokens;
                                    frame_cache_read = usage.cache_read_tokens;
                                    frame_cache_write = usage.cache_write_tokens;
                                    frame_output = usage.output_tokens;
                                    if let Some(cost) = usage.reported_cost {
                                        match authoritative_reported_micro(&cost) {
                                            Some(micro) => provider_reported_cost = Some(micro),
                                            None => {
                                                tracing::warn!(
                                                    session = %handle.id(),
                                                    "refusing non-USD provider-reported cost \
                                                     (currency {:?}, {micro} micro) as an \
                                                     authoritative settlement override",
                                                    cost.currency,
                                                    micro = cost.micro_usd,
                                                );
                                            }
                                        }
                                    }
                                }
                                Ok(ProviderChunk::Done) => break,
                                Err(e) => {
                                    // POST-dispatch stream failure (attempt
                                    // accounting): the provider may have
                                    // billed — the machine marks this
                                    // attempt UNCERTAIN with the failure
                                    // reason, so the reserved amount keeps
                                    // consuming the free budget until a
                                    // reconcile or the task-end finalize
                                    // (never a refund, never a dangling
                                    // dispatched row).
                                    if let Err(uncertain_err) = acct
                                        .fail_after_dispatch("provider_error", None)
                                        .await
                                    {
                                        tracing::error!(
                                            session = %handle.id(),
                                            "cannot mark the failed attempt uncertain: {uncertain_err}"
                                        );
                                    }
                                    // The credit hold stays durable for the
                                    // post-dispatch uncertainty window.
                                    debits.close_uncertain();
                                    // The failed attempt's durable provider-call row is
                                    // ATTEMPT-KEYED like its start row: this physical
                                    // attempt's failure with its own attempt identity
                                    // and reservation link — never a legacy row merged
                                    // under the shared logical op. The terminal tokens
                                    // are not recorded (a failed stream's partial
                                    // usage is not a durable spend basis; the
                                    // UNCERTAIN reservation resolves at reconcile or
                                    // the task-end finalize).
                                    handle.record_provider_call_attempt(
                                        attempt_identity,
                                        reservation_link(reservation),
                                        provider.id(),
                                        &model,
                                        "failed",
                                        None,
                                        None,
                                        Some(&e.to_string()),
                                    )?;
                                    // Retry ONLY when nothing durable happened in this
                                    // request (no flushed parts, no message created, no
                                    // tool runs pending) and the failure is retryable.
                                    let safe = assistant_message.is_none()
                                        && handle.pending_tool_runs()?.is_empty();
                                    // Class-aware: the same predicate the policy exposes
                                    // (Network never retries rate limits; RateLimited/
                                    // ServerError/Always do), instead of consulting only
                                    // `retryable`.
                                    let rate_limited = matches!(
                                        e.kind,
                                        faktor_provider::ProviderErrorKind::RateLimited
                                    );
                                    if safe
                                        && retry_policy
                                            .should_retry(attempt, e.retryable, rate_limited)
                                    {
                                        tracing::warn!(
                                        "provider failure on attempt {} of {max_attempts}: {e}; retrying",
                                        attempt + 1
                                    );
                                        // The failed request journaled nothing durable:
                                        // the wire state is unchanged — safe to retry.
                                        assistant_message = None;
                                        text_buf.clear();
                                        reasoning_buf.clear();
                                        tool_calls.clear();
                                        continue 'attempts;
                                    }
                                    // Telemetry outcome entry (P0-28): the
                                    // TERMINAL failure of the logical call —
                                    // resolved=false, with the retry/reliability
                                    // signals and the final attempt's latency.
                                    // Failure signal only: no verified sample
                                    // (a failed stream's task never reached a
                                    // deterministic gate in this call).
                                    self.deps.routing.record_call_outcome(
                                        &SettledCallOutcome {
                                            provider: provider.id().to_string(),
                                            model: model.clone(),
                                            phase: RouterPhase::Implement,
                                            success: false,
                                            retried: settled_attempt > 0,
                                            rate_limited: matches!(
                                                e.kind,
                                                ProviderErrorKind::RateLimited
                                            ),
                                            latency_ms: attempt_started
                                                .elapsed()
                                                .as_millis()
                                                .min(u64::MAX as u128) as u64,
                                            verified: None,
                                        },
                                    );
                                    return self
                                        .handle_provider_failure(handle, op_id, e, &mut outcome)
                                        .await;
                                }
                            }
                        }
                        _ = stall_ticks.tick() => {
                            if self.progress_stalled(handle.id()) {
                                // Stall verdict: silence past the budget while
                                // the op is in flight. Surfaced as an honest
                                // non-retryable provider failure so the turn
                                // ends in the SAME machine-safe way as any
                                // failed stream (never a blind re-run).
                                let err = ProviderError {
                                    kind: ProviderErrorKind::Timeout,
                                    message: format!(
                                        "stall detected: no output, progress or completed op within {} ms",
                                        self.stall_silence()
                                    ),
                                    retryable: false,
                                    code: None,
                                };
                                tracing::warn!("{err_message}", err_message = err.message);
                                outcome.loop_stopped = false;
                                outcome.stalled = true;
                                // POST-dispatch stall verdict: the request
                                // left the process and the provider may have
                                // billed — UNCERTAIN, never a refund.
                                if let Err(uncertain_err) = acct
                                    .fail_after_dispatch("stall_verdict", None)
                                    .await
                                {
                                    tracing::error!(
                                        session = %handle.id(),
                                        "cannot mark the stalled attempt uncertain: {uncertain_err}"
                                    );
                                }
                                // Post-dispatch stall: the credit hold stays
                                // durable (the provider may have billed).
                                debits.close_uncertain();
                                return self
                                    .handle_provider_failure(handle, op_id, err, &mut outcome)
                                    .await;
                            }
                        }
                    }
                }
                // This attempt consumed a full stream (clean end): settle
                // the reservation THROUGH the machine at the usage-frame
                // actual — the provider-reported cost when the frame
                // carried one (authoritative), else the frame's token
                // categories x the reservation's frozen route-time price
                // capture. Unpriced (no snapshot, no reported cost) the row
                // closes as a documented Unknown spend under no cap, or
                // fails typed under one — never a fabricated 1-micro
                // actual. A refused settle (e.g. UnknownPrice under a hard
                // cap) leaves the DISPATCHED row — the machine closes it
                // UNCERTAIN so the attempt keeps consuming until
                // reconcile/finalize instead of dangling.
                let settled_actual = match acct
                    .settle_usage(
                        frame_uncached_input,
                        frame_cache_read,
                        frame_cache_write,
                        frame_output,
                        provider_reported_cost,
                        route_json,
                    )
                    .await
                {
                    Ok(settled) => settled,
                    Err(settle_err) => {
                        tracing::warn!(
                            session = %handle.id(),
                            "reservation {reservation} settlement refused: {settle_err}; marking the attempt uncertain"
                        );
                        if let Err(uncertain_err) =
                            acct.fail_after_dispatch("settle_refused", None).await
                        {
                            tracing::error!(
                                session = %handle.id(),
                                "cannot mark the unsettled attempt uncertain: {uncertain_err}"
                            );
                        }
                        // The budget side refused the settle, so the credit
                        // hold must not move either: it stays durable for
                        // reconciliation (a fabricated actual would be worse
                        // than a pending hold).
                        debits.close_uncertain();
                        return Err(settle_err.into());
                    }
                };
                // The commercial credit hold follows the reservation
                // ledger's OWN settlement truth: a settled actual consumes
                // the hold at exactly that amount; a documented Unknown
                // spend (no price authority, no cap) leaves the hold durable
                // for reconciliation — never a fabricated zero settle.
                match settled_actual {
                    Some(actual_micro) => {
                        if let Err(e) = debits.settle(actual_micro) {
                            tracing::error!(
                                session = %handle.id(),
                                attempt = %attempt_identity.attempt_op_id,
                                "settlement of the managed credit hold at {actual_micro} micro failed (the hold stays durable for reconciliation): {e}"
                            );
                        }
                    }
                    None => debits.close_uncertain(),
                }
                break 'attempts;
            }

            if let Some(mid) = assistant_message {
                if !reasoning_buf.is_empty() {
                    handle.append_reasoning_part(mid, &reasoning_buf).await?;
                }
                if !text_buf.is_empty() {
                    handle.append_text_part(mid, &text_buf).await?;
                }
            }
            // The settled attempt's durable provider-call row (attempt
            // accounting, schema v18): ATTEMPT-KEYED with THIS physical
            // attempt's own identity and reservation link, and the canonical
            // usage of the wave-B2 frame (audit Phase-1 item C) — the input
            // fold the row persists (uncached + cache reads + cache writes)
            // and the output counter. Reconciliation of an UNCERTAIN
            // reservation joins `provider_call.attempt_op_id =
            // cost_reservation.attempt_op_id`, so a crashed attempt settles
            // from its OWN row — never from a sibling attempt's completed
            // row and never from a legacy logical-op merged row.
            if let (Some(attempt_identity), Some(reservation)) =
                (settled_attempt_identity, settled_reservation)
            {
                handle.record_provider_call_attempt(
                    attempt_identity,
                    reservation_link(reservation),
                    provider.id(),
                    &model,
                    "completed",
                    Some(tokens_in),
                    Some(tokens_out),
                    None,
                )?;
            }
            // Prefix-cache observation (audits 45/65-66 fill site,
            // architecture §8.4): the completed call additionally lands the
            // digest of the EXACT cacheable-prefix bytes the wire request
            // carried — the plan's StaticPrefix + SemiStable head
            // (`build_request` sends `plan.system` verbatim, so the plan
            // render IS the sent bytes) plus the head's estimated token
            // count. The volatile evidence/errors tail is excluded: volatile
            // churn must never be misread as prefix churn.
            // `settle_usage_with_prefix` derives the row's per-turn
            // stability against the session's previous observation and lands
            // the row durably. This is the prefix consumers' row (stability
            // history + routing consult); it carries NO usage counters — the
            // attempt-keyed row above is the usage record, so a legacy
            // merged row never double counts.
            //
            // Audit 45/82: the plan's per-call `PrefixObservation` carries
            // the eight conceptual segment digests/tokens and THIS call's
            // observed cache reads. The durable prefix row is the identity
            // the router's cache-economics consult reads back, so the exact
            // observation JSON rides the v19 additive payload of the twinned
            // settlement (`settle_usage_with_prefix_segments`): the router
            // later measures each turn's longest stable leading prefix
            // against its predecessor instead of approximating coverage from
            // the binary digest pair. The store validates the payload's
            // strict shape and bounds on write and read.
            let observation = wire_plan.prefix_observation(frame_cache_read);
            tracing::debug!(
                session = %handle.id(),
                segments = observation.segment_hashes.len(),
                stable_leading_tokens = observation
                    .longest_stable_prefix(None)
                    .stable_leading_tokens,
                cache_read_tokens = observation.cache_read_tokens,
                "prefix observation measured for router cache economics"
            );
            let (prefix_hash, prefix_tokens) = match wire_plan.cacheable_prefix() {
                Some(prefix) => (
                    Some(blake3::hash(prefix.as_bytes()).into()),
                    Some(faktor_context::Estimator.estimate_tokens(prefix) as u64),
                ),
                // The planner copies the head verbatim, so the boundary is
                // always a char boundary; on the impossible interior-splice
                // case record NO observation rather than hash the wrong
                // bytes (a missing observation is not a zero).
                None => (None, None),
            };
            if prefix_hash.is_some() {
                handle.settle_usage_with_prefix_segments(
                    op_id,
                    provider.id(),
                    &model,
                    "completed",
                    None,
                    None,
                    None,
                    prefix_hash,
                    prefix_tokens,
                    // PrefixObservation serialization is infallible for this
                    // value shape; the store re-validates regardless, so a
                    // hypothetical hostile value is refused loudly at the
                    // write instead of landing a degraded observation.
                    Some(&observation.to_json()),
                )?;
            }
            // P0-2 reconcile: this ATTEMPT's durable provider-call row is
            // now `completed`, so an UNCERTAIN reservation a crash left for
            // THIS SAME attempt settles FROM it — the completed call's
            // tokens at the crashed reservation's frozen snapshot. A crashed
            // sibling attempt (a different attempt id) never matches this
            // row; rows whose attempt never completes stay UNCERTAIN for the
            // task-completion finalize. Best-effort: a reconcile failure
            // never fails the settled call.
            if let Err(e) = self
                .deps
                .budgets
                .reconcile_uncertain(handle.id(), task_id)
                .await
            {
                tracing::warn!(
                    session = %handle.id(),
                    task = %task_id,
                    "uncertain-reservation reconcile after a settled call failed: {e}"
                );
            }
            // Telemetry outcome entry (P0-28): the SETTLED (resolved) call —
            // success=true with the actual provider/model, the retry signal
            // and the final attempt's latency. No verified signal exists at
            // this site ("the model said done" is not a verified success):
            // the outcome is RETAINED so the deterministic gate site of this
            // turn re-records it with the explicit verified attribution
            // (audit items 13/14/L — see finish_logical_turn).
            let settled_outcome = SettledCallOutcome {
                provider: provider.id().to_string(),
                model: model.clone(),
                phase: RouterPhase::Implement,
                success: true,
                retried: settled_attempt > 0,
                rate_limited: false,
                latency_ms: attempt_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                verified: None,
            };
            self.deps.routing.record_call_outcome(&settled_outcome);
            settled_calls.push((settled_outcome, drive_semantic_risk));

            // Stall signal (audit): several model iterations with NO new
            // durable state (no text/reasoning/tools) mean the agent is
            // buying tokens without progress — stop and re-plan instead.
            if detector.record_progress(iteration_progress, 8) {
                outcome.loop_stopped = true;
                outcome.stalled = true;
                // Classified end: the stall verdict must not be masked by a
                // journal error — record the lost Failed-journal.
                self.dw_note_journal_failed(
                    handle,
                    op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({
                        "message": "stall detected: repeated model iterations produced no new state"
                    }),
                    DW_SITE_STALL_JOURNAL,
                )
                .await;
                outcome.final_state = AgentState::FailedRecoverable;
                return Ok(outcome);
            }
            // Expensive-cycle stall (P0-79): the iteration boundary is a
            // model-call boundary — each cycle costs tokens. The predicate
            // consults SEMANTIC EVIDENCE (output, evidence classes, op
            // completion), never bare tool events: an op whose iterations
            // only churn tools (heartbeats) stalls once no evidence class
            // arrived within the budget — a tool event does not mean the
            // task is closer to completion. Mid-stream silence is caught
            // by the stream watchdog above (pure-silence, unchanged).
            if self.progress_stalled_evidence(handle.id()) {
                outcome.loop_stopped = false;
                outcome.stalled = true;
                // Classified end: the evidence-stall verdict must not be
                // masked by a journal error — record the lost Failed-journal.
                self.dw_note_journal_failed(
                    handle,
                    op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({
                        "message": "stall detected: tool activity without semantic evidence within the silence budget"
                    }),
                    DW_SITE_STALL_EVIDENCE_JOURNAL,
                )
                .await;
                outcome.final_state = AgentState::FailedRecoverable;
                return Ok(outcome);
            }
            // Iteration completion is progress evidence.
            self.progress_heartbeat(handle.id());
            if !tool_calls.is_empty() {
                // Phase boundary: mutating (DiskWrite) batches are Coding;
                // every other batch is Tool. The classification comes from
                // the tool registry's resource class — never from a name
                // string match.
                let batch_phase = if tool_calls.iter().any(|(_, name, _)| {
                    self.deps
                        .tools
                        .get(name)
                        .map(|t| {
                            t.resource_class == faktor_core::resource::ResourceClass::DiskWrite
                        })
                        .unwrap_or(false)
                }) {
                    ExecutionPhase::Coding
                } else {
                    ExecutionPhase::Tool
                };
                self.note_execution_phase(handle, batch_phase);
                let executed = self
                    .run_tool_calls(
                        handle,
                        &tool_bundle,
                        op_id,
                        &mut detector,
                        &mut ledger,
                        &mut turn_summary,
                        &cancel,
                        semantic_turn.as_ref(),
                        tool_calls,
                    )
                    .await?;
                // Durable cross-turn loop detection (spec §28): the same
                // failing calls repeated across logical turns trip here even
                // though each turn's LoopDetector starts fresh.
                let durable_trip = self.durable_loop_signals(handle, &turn_summary, &detector)?;
                // State-based loop fingerprints (P0-78): patch→revert→patch
                // and repeated evidence sets trip EVEN when tools executed
                // (execution success is not progress when the state
                // oscillates) — the typed code rides the outcome.
                let fingerprint_trip = detector.last_trip_code().is_some();
                if (executed == 0 && detector.trips > 0) || durable_trip || fingerprint_trip {
                    // Repeating failing calls / oscillating state: stop and
                    // re-plan.
                    outcome.loop_stopped = true;
                    let (code, detail) = match detector.last_trip_code() {
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
                    if detector.last_trip_code().is_some() {
                        outcome.stop_reason = Some(OutcomeReason::new(code, detail.to_string()));
                    }
                    // Classified end: the loop verdict must not be masked by a
                    // journal error — record the lost Failed-journal.
                    self.dw_note_journal_failed(
                        handle,
                        op_id,
                        AgentState::FailedRecoverable,
                        serde_json::json!({ "message": detail }),
                        DW_SITE_LOOP_JOURNAL,
                    )
                    .await;
                    // Typed ledger (audit 27): this genuine decision point —
                    // stop-and-replan — is durable history.
                    self.guarded_ledger_decision(
                        handle,
                        "replan",
                        "stop the turn and re-plan",
                        detail,
                        DW_SITE_LOOP_DECISION,
                    )?;
                    outcome.final_state = AgentState::FailedRecoverable;
                    return Ok(outcome);
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
                    self.walk_tool_batch_to_waiting(handle, op_id).await?;
                    continue; // stream again with tool results (machine at WaitingForModel)
                }
                if handle.state()? == AgentState::FailedRecoverable {
                    // executed == 0 with at least one submitted tool that
                    // failed recoverably: the turn's classified end. The
                    // machine is already at `FailedRecoverable` (the failed
                    // finishes), so this is an honest report, not a
                    // transition; the durable per-tool rows carry the
                    // outcomes. `FailedRecoverable -> Validating` is ILLEGAL
                    // by design — the old fall-through into the genuine-end
                    // tail died there with `InvalidState`.
                    outcome.final_state = AgentState::FailedRecoverable;
                    return Ok(outcome);
                }
                // executed == 0: every call was denied or unknown. If the
                // loop detector tripped we returned above; otherwise the
                // turn genuinely ends below (the denials already moved the
                // machine toward ReadyForNextTurn).
            }

            // ---- genuine end of the logical turn: validate → update
            // memory → ONE TurnCompleted → ReadyForNextTurn.
            self.genuine_end_tail(
                handle,
                op_id,
                &mut outcome,
                &mut ledger,
                &turn_summary,
                &mut settled_calls,
                &cancel,
            )
            .await?;
            return Ok(outcome);
        }
    }

    /// The shared genuine-end entry (audits 4/6/7 + audit 26 slice end):
    /// walks the machine legally into the end tail (a turn whose denials
    /// already landed ReadyForNextTurn skips the interior hops) and calls
    /// [`AgentRuntime::finish_logical_turn`] with the turn's cancellation
    /// token (the typed verification checks inherit its lineage).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn genuine_end_tail(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        outcome: &mut TurnOutcome,
        ledger: &mut TaskLedger,
        turn_summary: &faktor_context::ledger::TurnSummary,
        settled_calls: &mut Vec<(SettledCallOutcome, u8)>,
        cancel: &CancellationToken,
    ) -> faktor_core::Result<()> {
        let current = handle.state()?;
        if current != AgentState::ReadyForNextTurn {
            handle
                .append_journal_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    AgentState::Validating,
                    Some(op_id),
                    None,
                )
                .await?;
            handle
                .append_journal_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    AgentState::UpdatingMemory,
                    Some(op_id),
                    None,
                )
                .await?;
        }
        self.finish_logical_turn(
            handle,
            op_id,
            outcome,
            ledger,
            turn_summary,
            settled_calls,
            cancel,
        )
        .await
    }

    /// The single genuine-end tail shared by every end site (audits 4/6/7):
    /// fold the turn into the durable ledger, persist the memory rows, run
    /// the end-of-turn verification which classifies completion and writes
    /// the durable gate facts, journal the ONE TurnCompleted, sync the
    /// first-class durable Task row (audit 25) and report ReadyForNextTurn.
    /// `outcome.acceptance` is Fail for a failed verification but
    /// `outcome.final_state` STAYS ReadyForNextTurn — a failed verification
    /// never kills the session; the gate carries the non-completion. The
    /// turn's `cancel` token rides into the verification attempt so the
    /// typed checks inherit the turn's cancellation lineage (P0-9/10).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn finish_logical_turn(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        outcome: &mut TurnOutcome,
        ledger: &mut TaskLedger,
        turn_summary: &faktor_context::ledger::TurnSummary,
        settled_calls: &mut Vec<(SettledCallOutcome, u8)>,
        cancel: &CancellationToken,
    ) -> faktor_core::Result<()> {
        // P0-2 remainder: generic-shell changes are attributed through the
        // durable `shell_change` facts, so a crash-resumed turn (or a live
        // settlement that could not open a workspace) still feeds its ACTUAL
        // changed paths into the SAME consumers `write_file` feeds. An
        // unresolved attribution (captured/discovered/unattributed) refuses
        // the completion gate below — the unknown tree is never read as
        // "unchanged".
        let (durable_shell_paths, unresolved_shell) = shell_change_facts(handle, op_id);
        let mut merged_summary = turn_summary.clone();
        for path in durable_shell_paths {
            if !merged_summary.files_changed.contains(&path) {
                merged_summary.files_changed.push(path);
            }
        }
        let turn_summary = &merged_summary;
        ledger.record_turn(turn_summary);
        handle.put_task_ledger(serde_json::to_value(&*ledger)?)?;
        self.record_memory(handle, op_id, ledger, turn_summary)?;
        // The task finished with genuine work: loop windows close.
        if turn_made_progress(turn_summary) {
            self.guarded_reset_loop_signals(handle, DW_SITE_END_LOOP_SIGNALS)?;
        }
        // Verification-tier escalation (audit 54): the configured/default
        // quality for the turn, escalated to Strict when the semantic
        // consult reported High/Unknown risk. None (no provider consulted)
        // and non-escalating levels keep the quality byte-identical.
        let quality = verification_quality_for(
            outcome.semantic_risk,
            self.quality_for_turn(!turn_summary.files_changed.is_empty()),
        );
        let verdict = self
            .run_turn_verification(
                handle,
                op_id,
                &turn_summary.files_changed,
                &ledger.goal,
                quality,
                cancel,
            )
            .await;
        outcome.verification = verdict.verification;
        outcome.acceptance = verdict.acceptance;
        outcome.review = verdict.review;
        handle
            .append_journal_event(
                faktor_core::event::EventKind::TurnCompleted,
                AgentState::ReadyForNextTurn,
                Some(op_id),
                None,
            )
            .await?;
        outcome.turns += 1;
        outcome.final_state = AgentState::ReadyForNextTurn;
        // Durable Task row (audit 25 + P0-7/P0-8): the journaled end is
        // durable, so the spend counters (provider-call tokens +
        // turn_completed events) now include THIS turn. The CONTENT (goal /
        // criteria / plan / spend) is folded into the typed row first — no
        // state write yet — so the durable budget gate can refuse a PASSING
        // gate before any completion claim: a task whose spend already
        // exceeds its budget can NEVER reach VerifiedComplete. Only the
        // FINAL gate (after every durable refusal: budget, strict criteria)
        // drives the task-state machine and lands the completion proof.
        let mut gate = verdict.completion.clone();
        let synced = self.sync_task_row(handle, ledger, verdict.criteria.as_deref())?;
        if let Some(task) = &synced {
            if matches!(gate, Some(CompletionGate::VerifiedComplete)) && task_budget_exhausted(task)
            {
                gate = Some(CompletionGate::BlockedVerification {
                    reasons: vec![OutcomeReason::new(
                        ReasonCode::SpendOverBudget,
                        format!(
                            "task budget exhausted: max_tokens={:?} spent_tokens={}, max_turns={:?} spent_turns={}",
                            task.budget.max_tokens,
                            task.budget.spent_tokens,
                            task.budget.max_turns,
                            task.budget.spent_turns,
                        ),
                    )],
                });
                // Post-TurnCompleted tail: a refused gate's durable mirror
                // rows must not turn a completed turn into a failed one, so
                // each lost write is recorded (marker + audit) and replayed.
                self.dw_note_upsert_memory_fact(
                    handle,
                    FactSource::Durable,
                    "task_state",
                    "state",
                    "blocked",
                    DW_SITE_GATE_BUDGET_FACT,
                );
                // Typed ledger (audit 27): the budget refusal is a durable
                // decision with its rationale.
                self.dw_note_ledger_decision(
                    handle,
                    "completion gate",
                    "refuse VerifiedComplete",
                    "durable task budget exhausted; the gate is blocked until the budget allows",
                    DW_SITE_GATE_BUDGET_DECISION,
                );
            }
        }
        // Change-scope budget (audits 57/105): a mutating run that left the
        // task's durable ChangeBudget refuses the PASSING gate with the typed
        // reason. Runs after the spend refusal (both only ever downgrade a
        // passing gate; an existing blocker keeps precedence). No budget =
        // today's behavior. `semantic_entities: None` under a budget that
        // constrains semantic entities is the documented "Unknown => stronger
        // verification" refusal path.
        if synced.is_some() && matches!(gate, Some(CompletionGate::VerifiedComplete)) {
            let observations = faktor_session::budget::ChangeObservations {
                changed_paths: turn_summary.files_changed.clone(),
                ..Default::default()
            };
            match handle.enforce_change_budget(handle.task_id()?, &observations) {
                Ok(()) => {}
                Err(TaskError::ChangeBudgetRefused { violations, .. }) => {
                    gate = Some(CompletionGate::BlockedVerification {
                        reasons: vec![OutcomeReason::new(
                            ReasonCode::ChangeBudgetExceeded,
                            format!(
                                "the mutating run left the task's change budget: {violations:?}"
                            ),
                        )],
                    });
                    // Post-TurnCompleted tail: record, never propagate, so a
                    // refused gate cannot turn a completed turn into a failed
                    // one; replay reconstructs the mirror rows.
                    self.dw_note_upsert_memory_fact(
                        handle,
                        FactSource::Durable,
                        "task_state",
                        "state",
                        "blocked",
                        DW_SITE_GATE_CHANGE_FACT,
                    );
                    // Typed ledger (audit 27): the refusal is a durable
                    // decision with its rationale.
                    self.dw_note_ledger_decision(
                        handle,
                        "completion gate",
                        "refuse VerifiedComplete",
                        "the run's changed paths fall outside the task's change budget",
                        DW_SITE_GATE_CHANGE_DECISION,
                    );
                }
                Err(other) => return Err(other.into()),
            }
        }
        // Strict quality (the default for mutating turns, audit 92): the
        // durable criteria fact (`criteria`/`0`, wave-9 row) is verified
        // against the typed task row's acceptance criteria at this genuine
        // end. A disagreement refuses the completion claim — never silently
        // claims verified over rows that contradict each other. Runs AFTER
        // the content sync so a same-turn derivation already healed the crash
        // window; what remains is genuine divergence (hostile write or a
        // corrupted row).
        if self.quality_for_turn(!turn_summary.files_changed.is_empty())
            == VerificationQuality::Strict
        {
            if let Some(strict_gate) =
                self.enforce_criteria_consistency(handle, ledger, gate.clone())?
            {
                gate = Some(strict_gate);
                // Post-TurnCompleted tail: recorded (marker + audit), never
                // propagated — the refusal is the genuine outcome.
                self.dw_note_upsert_memory_fact(
                    handle,
                    FactSource::Durable,
                    "task_state",
                    "state",
                    "blocked",
                    DW_SITE_GATE_CRITERIA_BLOCK_FACT,
                );
                // Typed ledger (audit 27): the refusal is a durable decision.
                self.dw_note_ledger_decision(
                    handle,
                    "completion gate",
                    "refuse the completion claim",
                    "durable criteria rows disagree (criteria fact vs typed task row); deterministic re-derivation on a later turn converges",
                    DW_SITE_GATE_CRITERIA_BLOCK_DECISION,
                );
            }
        }
        // The FINAL gate drives the typed task row ONCE through the legal
        // state-machine edges; a VerifiedComplete gate additionally lands the
        // durable per-attempt VerificationRecord BEFORE complete_verified_task
        // (record-first). A typed completion refusal (the task row moved
        // between the record's certification and the completion transaction)
        // downgrades the gate — never fails the turn — and the fact is
        // rewritten to the refused gate below.
        if unresolved_shell {
            // P0-2 remainder: a shell mutation whose actual tree was never
            // certified blocks a completion claim with the typed
            // `unattributed_change` reason. The discovered/merged paths still
            // ran through verification above (evidence is never discarded);
            // only the CLAIM is refused until a re-verified turn succeeds.
            let detail = format!(
                "{} change(s) are attributed to this turn but at least one generic-shell \
                 mutation was never certified by the bounded pre/post manifest reconciliation; \
                 the completion claim is refused and the changes stay unattributed",
                turn_summary.files_changed.len()
            );
            gate = refuse_unattributed_gate(gate, &detail);
        }
        let gate_landed =
            self.apply_gate_to_task_row(handle, gate.clone(), verdict.proof.as_ref())?;
        if gate_landed.is_none() && matches!(gate, Some(CompletionGate::VerifiedComplete)) {
            // P0-2 task-completion backstop: the task is terminal, so every
            // outstanding UNCERTAIN reservation (a crashed daemon may have
            // dispatched it and the provider may have billed) settles
            // conservatively AT its reserved estimate — the honest bound the
            // ledger already committed to. Idempotent; best-effort (a
            // finalize failure never rewinds the completion).
            let task_id = handle.task_id()?;
            match self
                .deps
                .budgets
                .finalize_uncertain(handle.id(), task_id)
                .await
            {
                Ok(report) if report.settled > 0 => {
                    tracing::info!(
                        session = %handle.id(),
                        task = %task_id,
                        "task completion finalized {settled} uncertain reservation(s) at their \
                         reserved estimates ({charged} micro total)",
                        settled = report.settled,
                        charged = report.charged_micro,
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        session = %handle.id(),
                        task = %task_id,
                        "task-completion uncertain finalize failed: {e}"
                    );
                }
            }
        }
        if let Some(downgrade) = gate_landed {
            gate = Some(downgrade);
            // Post-TurnCompleted tail: recorded (marker + audit), never
            // propagated — the typed refusal is the genuine outcome.
            self.dw_note_upsert_memory_fact(
                handle,
                FactSource::Durable,
                "task_state",
                "state",
                "blocked",
                DW_SITE_GATE_DOWNGRADE_FACT,
            );
            // Typed ledger (audit 27): the refusal is a durable decision.
            self.dw_note_ledger_decision(
                handle,
                "completion gate",
                "refuse the completion claim",
                "the typed task row refused the completion proof (typed refusal: revision moved or the row is terminal); a later verification attempt converges with a fresh record",
                DW_SITE_GATE_DOWNGRADE_DECISION,
            );
        }
        outcome.completion = gate.clone();
        // Durable learning hooks (audits 65-67/92): the FINAL gate (after
        // every refusal/downgrade) is the only site that records a failed
        // attempt or mines a verified recovery. Disabled with the
        // `failure_learning` flag (additive: default runs write no learning
        // rows); errors are loud, never a swallowed durable gap.
        self.record_learning_for_gate(handle, gate.as_ref())
            .map_err(|error| Error::internal(format!("durable learning hook failed: {error}")))?;
        // Verified-outcome attribution (audit items 13/14/L fill site): the
        // deterministic gate verdict of this turn is the runtime's ONLY
        // verified signal, and it exists only on completion-claiming turns.
        // Every settled Implement call of the turn is re-recorded with the
        // explicit verified attribution: a VerifiedComplete gate carries the
        // verified-success signal (a first-pass-success sample keyed by the
        // settled call's provider/model/phase and the turn's attributed task
        // class/risk); every other gate verdict is a FAILURE sample (the
        // settled calls needed rework — never a success). Calls of turns
        // that never reached a gate carry no sample at all. Rework sums are
        // not attributable at this site yet (0 = unmeasured); routing treats
        // a failed-verification history with the documented escalation fallback.
        if let Some(gate) = &gate {
            for (settled, risk) in settled_calls.drain(..) {
                let mut attributed = settled;
                attributed.verified = Some(VerifiedCallAttribution {
                    // The runtime has no per-task class dimension (every
                    // task is a goal-driven coding task on this graph); the
                    // class key stays the honest Medium default.
                    task_class: TaskClass::Medium,
                    risk_bucket: risk_bucket_of(risk),
                    verified_success: matches!(gate, CompletionGate::VerifiedComplete),
                    rework_cost_micro: 0,
                    rework_turns: 0,
                });
                self.deps.routing.record_call_outcome(&attributed);
            }
        } else {
            settled_calls.clear();
        }
        // Typed durable ledger (audit 27): the genuine end's durable tail —
        // criteria, failures, the VerifyRun, the completion gate's blockers
        // and the TurnCompleted mirror. Appended AFTER the budget gate so
        // the blocker set matches the FINAL gate. Loud: the typed ledger
        // never silently drops a durable fact.
        self.typed_ledger_turn_end(
            handle,
            op_id,
            turn_summary,
            verdict.criteria.as_deref(),
            &outcome.verification,
            gate.as_ref(),
        )?;
        self.fire_task_complete_hook(handle, op_id, outcome);
        Ok(())
    }

    /// Drive-start typed-ledger heal + epoch detection (audit 27): seed
    /// GoalSet/CriteriaSet from the legacy blob / typed task rows when the
    /// typed stream predates them, and record `EpochBumped` when the
    /// instructions loader's epoch differs from the ledger (rule files
    /// changed across a reload/restart). The materialized head is refreshed
    /// after any append so nothing re-fires.
    pub(crate) fn typed_ledger_drive_start(
        &self,
        handle: &faktor_session::SessionHandle,
        ledger: &TaskLedger,
    ) -> faktor_core::Result<()> {
        let view = handle.ledger_view()?;
        let mut appended = false;
        if view.head.goal.is_empty()
            && !ledger.goal.is_empty()
            && handle
                .ledger_goal_set(&truncate(&ledger.goal, 4096))?
                .is_some()
        {
            appended = true;
        }
        if view.head.criteria.is_empty() {
            if let Some(task) = self.session_task(handle)? {
                if !task.acceptance_criteria.is_empty() {
                    let canonical = criteria_canonical_text(&task.acceptance_criteria);
                    if handle
                        .ledger_criteria_set(&task.acceptance_criteria, &canonical)?
                        .is_some()
                    {
                        appended = true;
                    }
                }
            }
        }
        // The instruction epoch of the session's DURABLE workspace root
        // (P0-32): no durable root -> no epoch row; a hostile tree is a
        // surfaced warn, never a silently wrong epoch. An epoch row is only
        // recorded once the environment is RULES-BEARING (or an epoch row
        // already exists, so later rule deletions still move the stamp) —
        // a vacuous empty-tree epoch never pollutes the ledger.
        if let Some((epoch, has_rules)) = self
            .session_instruction_epoch_strict(handle)
            .map_err(|e| {
                faktor_core::Error::new(
                    faktor_core::ErrorKind::Store,
                    format!(
                        "workspace instruction root is unavailable; refusing to end the turn under unverified instructions: {e}"
                    ),
                )
            })?
        {
            let recordable = view.head.epoch.is_some() || has_rules;
            if recordable
                && view.head.epoch != Some(epoch)
                && handle
                    .ledger_epoch_bumped(view.head.epoch, epoch)?
                    .is_some()
            {
                appended = true;
            }
        }
        if appended {
            handle.ledger_ensure_head()?;
        }
        Ok(())
    }

    /// The typed durable ledger tail of every genuine turn end (audit 27):
    /// the criteria rows, recorded failures, the VerifyRun (checks +
    /// pass/fail), the completion gate's blockers (opened on
    /// Blocked/FailedVerification; resolved when a later turn verifies
    /// complete) and the TurnCompleted mirror. Loud errors: the typed
    /// ledger never silently drops a durable fact.
    pub(crate) fn typed_ledger_turn_end(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        summary: &faktor_context::ledger::TurnSummary,
        criteria: Option<&[String]>,
        verification: &[(String, bool)],
        gate: Option<&CompletionGate>,
    ) -> faktor_core::Result<()> {
        if let Some(criteria) = criteria {
            if !criteria.is_empty() {
                let canonical = criteria_canonical_text(criteria);
                handle.ledger_criteria_set(criteria, &canonical)?;
            }
        }
        for failure in summary.failures.iter().take(32) {
            handle.ledger_failure_recorded(&truncate(failure, 4096))?;
        }
        if !verification.is_empty() {
            let checks: Vec<faktor_session::LedgerCheckRun> = verification
                .iter()
                .map(|(id, passed)| faktor_session::LedgerCheckRun {
                    id: truncate(id, 128),
                    passed: *passed,
                })
                .collect();
            let outcome = match gate {
                Some(CompletionGate::VerifiedComplete) => "passed",
                Some(CompletionGate::FailedVerification { .. }) => "failed",
                Some(CompletionGate::BlockedVerification { .. }) => "blocked",
                Some(CompletionGate::Unverified) => "unverified",
                Some(CompletionGate::VerificationPending) => "pending",
                None => "pending",
            };
            handle.ledger_verify_run(&checks, outcome)?;
        }
        match gate {
            Some(CompletionGate::BlockedVerification { reasons })
            | Some(CompletionGate::FailedVerification { reasons }) => {
                for reason in reasons.iter().take(16) {
                    handle.ledger_blocker_opened(&truncate(&reason.detail, 4096))?;
                }
            }
            Some(CompletionGate::VerifiedComplete) => {
                // A later verified-complete turn resolves every blocker
                // that was open (their reasons no longer block).
                let view = handle.ledger_view()?;
                for reason in view.head.open_blockers {
                    handle.ledger_blocker_resolved(&reason)?;
                }
            }
            Some(CompletionGate::Unverified) | Some(CompletionGate::VerificationPending) | None => {
            }
        }
        handle.ledger_turn_completed(op_id.raw())?;
        Ok(())
    }

    /// End-of-turn verification + completion gating (audits 4/6/7 — the
    /// typed-verifier migration P0-9/10 — verification must not depend on
    /// the model's discretion and must not be advisory): derive the checks
    /// this turn's OWN file changes require from the bounded repo file map
    /// (same root resolution as the repo-knowledge walk), execute the
    /// REQUIRED ones through the wired typed service under policy budgets,
    /// durably record one memory fact per failed required check, classify
    /// the completion gate and write the durable gate rows. Only the two
    /// genuine turn ends (the sites that record ledger/memory, via
    /// [`AgentRuntime::finish_logical_turn`]) call it.
    ///
    /// Typed execution (P0-9/10) — the legacy string-`sh -c` runner and its
    /// fixed 30 s/check + 10 s wall caps are GONE:
    /// - derivation is multi-component (`derive::detect_project_profile` +
    ///   `derive::derive_checks`): every changed file maps to its owning
    ///   component by longest root prefix, and every owning component
    ///   contributes ALL of its applicable typed `(program, argv)`
    ///   [`faktor_verify::exec::CheckSpec`] families. The single-project
    ///   `ProjectType` first-match slice no longer decides what runs; cap or
    ///   id conflicts are typed refusals (never silent truncation);
    /// - every required check runs under the service's policy budget
    ///   (`budget_for`): Quick ≤ 60 s class, Unit up to the unit cap, Full
    ///   background-by-policy. A "task-owned background operation" decision
    ///   on the real supervisor-backed service is a DURABLE background
    ///   verification job (audit P0-5/26): the required check is persisted
    ///   as a Queued job row, the attempt record freezes the derivation
    ///   order and inline outcomes, the task stays Verifying
    ///   ([`CompletionGate::VerificationPending`]), and the settlement pass
    ///   of a later genuine end executes every job through the supervisor
    ///   executor and resolves it (CAS) — completion proceeds only when all
    ///   required jobs are terminal (records built from job results);
    ///   scripted command backends (test seams) run the decision inline;
    /// - checks execute in the session's DURABLE workspace root with the
    ///   turn's cancellation lineage (child token): the daemon's current
    ///   directory is never consulted and never used as the check cwd.
    ///
    /// Gating matrix (each row assumes the turn changed files):
    /// - service wired AND the change derives required checks AND every
    ///   required check ran and passed AND the review does not block →
    ///   [`CompletionGate::VerifiedComplete`] (`task_state`
    ///   VerifiedComplete; `verification` status Passed).
    /// - a required check RAN and FAILED (status Failed) →
    ///   [`CompletionGate::FailedVerification`] with reasons naming the
    ///   check (`check_failed`); acceptance Fail; the session stays usable.
    /// - a required check could NOT run (execution Unavailable: killed by
    ///   deadline/cancellation, program missing, bridge rejection, infra
    ///   error) → [`CompletionGate::BlockedVerification`] with
    ///   `required check '<id>' unavailable` (`check_unavailable`).
    /// - the review gates the change while the checks passed →
    ///   [`CompletionGate::BlockedVerification`] with the review's reasons
    ///   (`review_blocked`; skeptical-review gate for high-impact work).
    ///   Quality decides the review bar: Normal only a verdict `"block"`
    ///   gates; Strict (the mutating-turn default) also gates non-`"pass"`
    ///   verdict shapes and advisory suspects (see
    ///   [`VerificationQuality`]).
    /// - the service is [`crate::VerificationService::disabled`] entirely →
    ///   Unverified, with a warning EACH mutating turn (documented: no
    ///   objective mechanism configured) — mutating turns without a
    ///   verifier are NEVER silently complete.
    /// - service present but the workspace/repo does not resolve, or no
    ///   check derives for the change → Unverified (nothing objective ran).
    ///
    /// Infra absence NEVER fails the turn itself: the completion gate carries
    /// the non-completion and `final_state` stays ReadyForNextTurn.
    pub(crate) async fn run_turn_verification(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        changed: &[String],
        goal: &str,
        quality: VerificationQuality,
        cancel: &CancellationToken,
    ) -> TurnEndVerdict {
        // ---- durable background-jobs boundary (audit P0-5/26) ----
        // A genuine end whose task still carries OPEN verification jobs acts
        // on them FIRST (only the real supervisor-backed service can have
        // persisted jobs):
        //   - nothing changed this turn: the open jobs are the only open
        //     claim — recovery + execution + resolution; when every
        //     required job of the newest attempt is terminal the existing
        //     completion path proceeds from the job results (records built
        //     from job results);
        //   - this turn changed files: the old attempt's content moved, so
        //     its open jobs are superseded (typed Cancelled rows — never
        //     silently dropped) and the fresh derivation below claims the
        //     current content. The task NEVER completes from a vanished or
        //     superseded process: VerifiedComplete arrives only through a
        //     settled, terminal job set.
        let background_service = self.deps.verification.can_persist_jobs();
        if background_service {
            // P2-VERIFY (audit): these reads gate settlement. A store failure
            // must not silently skip the whole background path; it blocks the
            // completion typed instead.
            let task_id = match handle.row() {
                Ok(row) => row.task_id.raw(),
                Err(e) => {
                    tracing::error!(error = %e, "turn-end settlement skipped: session row unreadable");
                    if changed.is_empty() {
                        return TurnEndVerdict::default();
                    }
                    return self.unverified_verdict(
                        handle,
                        changed,
                        None,
                        "session row unreadable; verification cannot be settled",
                    );
                }
            };
            {
                match handle.open_verification_jobs(task_id) {
                    Ok(open) if !open.is_empty() => {
                        if changed.is_empty() {
                            return self.settle_verification_jobs(handle, cancel).await;
                        }
                        match handle.current_verification_attempt(task_id) {
                            Ok(Some(attempt)) => {
                                let note = format!(
                                    "superseded by the newer verification attempt of turn op {} \
                                     (its content moved; the open jobs were never certified)",
                                    op_id.raw()
                                );
                                // The verdict function is infallible: the supersede
                                // cancel is recorded (marker + audit), not
                                // propagated.
                                self.dw_note_cancel_verification_attempt(
                                    handle,
                                    task_id,
                                    attempt.op_id,
                                    &note,
                                    DW_SITE_SUPERSEDE_CANCEL,
                                );
                            }
                            Ok(None) => {}
                            Err(e) => {
                                tracing::error!(error = %e, "verification attempt unreadable; refusing to complete this turn");
                                return self.unverified_verdict(
                                    handle,
                                    changed,
                                    None,
                                    "verification attempt unreadable; completion is blocked",
                                );
                            }
                        }
                    }
                    Ok(_) => {
                        // No OPEN job remains, but the newest attempt may
                        // have been resolved by the DAEMON verification
                        // executor and not yet consumed: a non-terminal task
                        // with a durable current attempt settles from its
                        // exact rows (the executor never settles the task
                        // itself).
                        if changed.is_empty() {
                            let non_terminal = match handle.task_id() {
                                Ok(t) => match handle.get_task(t) {
                                    Ok(Some(task)) => !task.state.is_terminal(),
                                    Ok(None) => false,
                                    Err(e) => {
                                        tracing::error!(error = %e, "task row unreadable; refusing to complete this turn");
                                        return self.unverified_verdict(
                                            handle,
                                            changed,
                                            None,
                                            "task row unreadable; completion is blocked",
                                        );
                                    }
                                },
                                Err(e) => {
                                    tracing::error!(error = %e, "task id unreadable; refusing to complete this turn");
                                    return self.unverified_verdict(
                                        handle,
                                        changed,
                                        None,
                                        "task id unreadable; completion is blocked",
                                    );
                                }
                            };
                            let attempt_exists = match handle.current_verification_attempt(task_id)
                            {
                                Ok(attempt) => attempt.is_some(),
                                Err(e) => {
                                    tracing::error!(error = %e, "verification attempt unreadable; refusing to complete this turn");
                                    return self.unverified_verdict(
                                        handle,
                                        changed,
                                        None,
                                        "verification attempt unreadable; completion is blocked",
                                    );
                                }
                            };
                            if non_terminal && attempt_exists {
                                return self.settle_verification_jobs(handle, cancel).await;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "open verification jobs unreadable; refusing to complete this turn");
                        // No change this turn means there is no completion
                        // claim to gate: keep the documented no-claim path.
                        if changed.is_empty() {
                            return TurnEndVerdict::default();
                        }
                        return self.unverified_verdict(
                            handle,
                            changed,
                            None,
                            "open verification jobs unreadable; completion is blocked",
                        );
                    }
                }
            }
        }
        // Nothing this turn changed: there is no completion claim to gate —
        // no verification runs and the gate stays unset.
        if changed.is_empty() {
            return TurnEndVerdict::default();
        }
        if self.deps.verification.is_disabled() {
            return self.unverified_verdict(
                handle,
                changed,
                None,
                "no verifier configured (no objective mechanism for this deployment)",
            );
        }
        let row = match handle.row() {
            Ok(r) => r,
            Err(_) => {
                return self.unverified_verdict(handle, changed, None, "session row unresolvable")
            }
        };
        // P0-48 root re-pointing: the turn's verification runs against the
        // session's EFFECTIVE root (the live shadow root of a shadowed
        // drive), so checks execute over the shadow world the turn mutated —
        // never the daemon cwd and never a stale user checkout. Un-shadowed
        // sessions resolve the stored workspace root byte-identically.
        let root = match self.deps.session.resolve_workspace_root(handle.id()) {
            Ok(Some(r)) => r,
            _ => {
                return self.unverified_verdict(
                    handle,
                    changed,
                    None,
                    "session workspace root unresolvable",
                )
            }
        };
        let ws = match self.deps.workspaces.open(row.workspace_id, root.clone()) {
            Ok(w) => w,
            Err(_) => {
                return self.unverified_verdict(
                    handle,
                    changed,
                    None,
                    "workspace could not be opened for verification",
                )
            }
        };
        // Bounded repository discovery with an EXPLICIT completeness verdict
        // (audit: a partial repo view can never derive a smaller passing
        // suite). ANY non-Complete inventory classifies the turn Unavailable
        // with the typed reason BEFORE any review or derivation: the model's
        // claim is never gated by checks derived from a truncated tree.
        let inventory = match &self.deps.supervisor {
            Some(supervisor) => faktor_verify::discover_repo_inventory_with_supervisor(
                &root,
                &faktor_verify::InventoryBudget::default(),
                supervisor,
            ),
            None => faktor_verify::discover_repo_inventory(&root),
        };
        if let Some(reason) = inventory.refusal_reason() {
            return self.unverified_verdict(
                handle,
                changed,
                None,
                &format!("repository inventory incomplete: {reason}"),
            );
        }
        let repo_files: Vec<String> = inventory.files;
        // Independent completion review (audit round 15, P0-12/80 + P0-13):
        // the legacy bounded head scan PLUS a structured diff package built
        // from the checkpoint/CAS base (hunks, statuses, inventory delta)
        // and — for RISKY changes only — a real separate review-model call
        // through the routing policy (phase Review) with a context-isolated
        // request. Advisory: never fails the turn itself, but a blocking
        // verdict downgrades the completion gate below VerifiedComplete.
        // Phase boundaries: verification owns the turn; the isolated review
        // call is the Review phase, then verification resumes.
        self.note_execution_phase(handle, ExecutionPhase::Verifying);
        self.note_execution_phase(handle, ExecutionPhase::Review);
        let review = independent_completion_review(
            self.deps.as_ref(),
            handle,
            &ws,
            changed,
            goal,
            &repo_files,
            cancel,
        )
        .await;
        self.note_execution_phase(handle, ExecutionPhase::Verifying);
        if repo_files.is_empty() {
            return self.unverified_verdict(
                handle,
                changed,
                review,
                "repository file map empty (no project type detectable)",
            );
        }
        // Multi-component derivation (audit: single-project verification):
        // every detected component owns its changed files by longest root
        // prefix and contributes ALL of its applicable check families — the
        // legacy first-match ProjectType slice is gone from the runtime. The
        // caps are typed refusals, never silent truncation of required
        // semantic coverage. `checks` is the legacy mirror the criteria rows,
        // durable facts, gate reasons and proof records consume (canonical
        // command text included); `specs_by_id` is what actually EXECUTES.
        let profile = faktor_verify::derive::detect_project_profile(&root, &repo_files);
        let changed_paths: Vec<std::path::PathBuf> =
            changed.iter().map(std::path::PathBuf::from).collect();
        let specs = match faktor_verify::derive::derive_checks(&profile, &changed_paths) {
            Ok(specs) => specs,
            Err(e) => {
                // A cap/ownership refusal can never certify completion: the
                // required semantic coverage could not be derived without
                // truncation, so the turn stays Unverified.
                return self.unverified_verdict(
                    handle,
                    changed,
                    review,
                    &format!("verification derivation refused: {e}"),
                );
            }
        };
        if specs.is_empty() {
            // No check applies to this change: the objective mechanism
            // exists but confirms nothing — Unverified, never a claim of
            // completion.
            return self.unverified_verdict(
                handle,
                changed,
                review,
                "no derived checks apply to this change",
            );
        }
        let checks: Vec<faktor_verify::Check> = specs.iter().map(legacy_mirror_of_spec).collect();
        let mut specs_by_id: std::collections::HashMap<
            String,
            Result<faktor_verify::exec::CheckSpec, String>,
        > = std::collections::HashMap::new();
        for spec in specs {
            specs_by_id.insert(spec.id.clone(), Ok(spec));
        }
        // The once-only acceptance-criteria rows: goal + the derived
        // required checks, frozen at the first sighting. Memory facts are
        // durable rows compaction NEVER rewrites; the typed task row is
        // seeded from the SAME canonical entries. The goal is NOT an
        // automatic criterion: acceptance criteria are the derived required
        // checks with their typed bindings.
        let criteria = criteria_rows(goal, &checks);

        // workspace root — the live shadow root of a shadowed drive, else
        // the durable workspace root (never the daemon cwd), the turn's
        // identity and a CHILD of the turn's cancellation token (the turn's
        // cancel aborts in-flight checks; each check additionally runs
        // under its own policy budget deadline, set below).
        let base_ctx = faktor_verify::exec::VerificationContext {
            session_id: handle.id().raw(),
            task_id: row.task_id.raw(),
            operation_id: op_id.raw(),
            workspace_id: row.workspace_id.raw(),
            worktree_id: row.worktree_id.raw(),
            root: root.clone(),
            deadline: std::time::Instant::now(),
            cancellation: cancel.child(),
        };
        let service = self.deps.verification.clone();
        let mut results: Vec<(String, bool)> = Vec::new();
        let mut unavailable: Vec<(String, String)> = Vec::new();
        // One typed execution row per required check that RAN (the P0-8
        // proof): real program/argv/exit/summary/timestamps from the
        // CheckOutcome — never a whitespace re-split of a shell string.
        let mut executed: Vec<ExecutedCheck> = Vec::new();
        // Durable background-attempt evidence (audit P0-5/26): when ANY
        // required check of this attempt must run as a persisted job, the
        // whole attempt becomes a background attempt — every required check
        // is captured in derivation order (inline outcomes + job
        // definitions) so a later settlement can rebuild the complete
        // result set from durable rows alone.
        let mut ordered_checks: Vec<String> = Vec::new();
        let mut inline_outcomes: std::collections::HashMap<String, CheckRunStatus> =
            std::collections::HashMap::new();
        let mut job_defs: Vec<(faktor_verify::Check, faktor_verify::exec::CheckSpec)> = Vec::new();
        for check in checks.iter().filter(|c| c.required) {
            let id = check.id.clone();
            let command = check.command.clone();
            ordered_checks.push(id.clone());
            let spec = match specs_by_id.get(&id) {
                Some(Ok(spec)) => spec.clone(),
                Some(Err(reason)) => {
                    // Bridge rejection (shell metacharacters/quotes): the
                    // check could not run — never executed through sh -c.
                    tracing::warn!(
                        "session {}: required check '{}' rejected by the typed bridge: {reason}",
                        handle.id(),
                        id
                    );
                    unavailable.push((id, command));
                    continue;
                }
                None => {
                    tracing::error!(
                        "session {}: required check '{}' has no typed spec",
                        handle.id(),
                        id
                    );
                    unavailable.push((id, command));
                    continue;
                }
            };
            // Policy budget (P0-10): per-category caps, no universal wall
            // cap. A "background" decision on the REAL supervisor-backed
            // service is a DURABLE background verification job (audit
            // P0-5/26): persisted Queued now, executed by the settlement
            // pass at a later genuine end — never executed inline under a
            // silent override. The scripted command backend (test seams;
            // instantaneous deterministic verdicts) runs background
            // decisions inline under the policy's unit cap.
            let budget = match service.budget_for(&spec) {
                BudgetDecision::RunInline(budget) => budget,
                BudgetDecision::RunAsTaskOwnedOperation if !background_service => {
                    service.policy().unit_max
                }
                BudgetDecision::RunAsTaskOwnedOperation => service.policy().unit_max,
            };
            let background_job = background_service
                && matches!(
                    service.budget_for(&spec),
                    BudgetDecision::RunAsTaskOwnedOperation
                );
            if background_job {
                if budget.is_zero() {
                    // Fail closed: the policy leaves no budget at all (a job
                    // with no deadline would be unbounded — never).
                    unavailable.push((id, command));
                    continue;
                }
                job_defs.push((check.clone(), spec.clone()));
                continue;
            }
            if budget.is_zero() {
                // Fail closed: the policy leaves no inline budget at all.
                unavailable.push((id, command));
                continue;
            }
            let mut vctx = base_ctx.clone();
            vctx.deadline = std::time::Instant::now() + budget;
            let outcome = service.execute(&spec, &vctx).await;
            inline_outcomes.insert(id.clone(), outcome.status);
            match outcome.status {
                // The check executed and passed.
                CheckRunStatus::Passed => {
                    results.push((id, true));
                    executed.push(executed_check_row(check, &spec, &outcome));
                }
                // The check executed and failed.
                CheckRunStatus::Failed => {
                    results.push((id, false));
                    executed.push(executed_check_row(check, &spec, &outcome));
                }
                // No verdict (deadline/cancellation kill, program missing,
                // infra): the check could not run.
                CheckRunStatus::Unavailable => unavailable.push((id, command)),
            }
        }
        // ---- background-attempt tail (audit P0-5/26) ----
        // When required checks of this attempt were persisted as jobs, the
        // gate cannot certify completion this turn: the attempt is durable
        // (job rows + one ordered attempt record) and the task parks at
        // Verifying until a later genuine end settles every job. Exceptions
        // that make the whole attempt moot BEFORE any job runs:
        //   - an inline required check FAILED (the attempt is failed);
        //   - the completion review blocks (the gate is Blocked).
        // In both cases the enqueued jobs never begin (nothing durable was
        // written yet) and the existing gate tail classifies the turn.
        let inline_failed = results.iter().any(|(_, ok)| !ok);
        let review_blocked = !review_blocking_reasons(review.as_ref(), quality).is_empty();
        if background_service && !job_defs.is_empty() && !(inline_failed || review_blocked) {
            // Persist the attempt: ordered required checks + job rows. A
            // typed refusal (hostile oversized spec/root, an open job of a
            // crashed earlier attempt) turns every job of this attempt into
            // an unavailable check — the attempt NEVER silently vanishes.
            let checks_ordered: Vec<faktor_session::VerificationAttemptCheck> = ordered_checks
                .iter()
                .map(|id| {
                    let command = checks
                        .iter()
                        .find(|c| &c.id == id)
                        .map(|c| c.command.clone())
                        .unwrap_or_default();
                    let is_job = job_defs.iter().any(|(c, _)| &c.id == id);
                    let inline = if is_job {
                        None
                    } else {
                        Some(match inline_outcomes.get(id) {
                            Some(CheckRunStatus::Passed) => {
                                faktor_session::VerificationInlineStatus::Passed
                            }
                            Some(CheckRunStatus::Failed) => {
                                faktor_session::VerificationInlineStatus::Failed
                            }
                            // Unavailable inline (deadline kill, bridge
                            // rejection, zero budget): recorded honestly —
                            // the check produced no verdict.
                            _ => faktor_session::VerificationInlineStatus::Unavailable,
                        })
                    };
                    faktor_session::VerificationAttemptCheck {
                        check_id: id.clone(),
                        command,
                        inline,
                    }
                })
                .collect();
            let job_inputs: Vec<faktor_session::VerificationJobInput> = job_defs
                .iter()
                .map(|(check, spec)| faktor_session::VerificationJobInput {
                    check_id: check.id.clone(),
                    kind: format!("{:?}", check.kind).to_ascii_lowercase(),
                    command: check.command.clone(),
                    // Bounded argv identity (exact execution still comes
                    // from the typed spec JSON, so a non-UTF8 argv is never
                    // lost: this view is the durable bound/index only).
                    program: spec.program.to_string_lossy().into_owned(),
                    args: spec
                        .args
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect(),
                    spec_json: serde_json::to_string(spec).unwrap_or_default(),
                    budget_ms: service.policy().unit_max.as_millis() as u64,
                })
                .collect();
            // Schema v2 (audits 94/116/117): fingerprint the environment the
            // background jobs are enqueued under; it rides the durable
            // attempt record AND every job row, so a job settled after a
            // restart still knows what it was enqueued under. The check basis
            // is the ordered derivation with the typed spec when one parsed
            // (the same program/argv the job executes). Infallible: the
            // enqueue never blocks on auxiliary evidence.
            let check_basis: Vec<(String, String, Vec<String>)> = ordered_checks
                .iter()
                .map(|id| {
                    let command = checks
                        .iter()
                        .find(|c| &c.id == id)
                        .map(|c| c.command.clone())
                        .unwrap_or_default();
                    match specs_by_id.get(id) {
                        Some(Ok(spec)) => (
                            id.clone(),
                            spec.program.to_string_lossy().into_owned(),
                            spec.args
                                .iter()
                                .map(|a| a.to_string_lossy().into_owned())
                                .collect(),
                        ),
                        _ => (id.clone(), command, Vec::new()),
                    }
                })
                .collect();
            let (enqueue_fingerprint, _, _) =
                self.observe_environment_fingerprint(handle, row.task_id, &check_basis, Some(&ws));
            let begin = handle.begin_verification_attempt_with_fingerprint(
                row.task_id.raw(),
                handle
                    .task_revision(row.task_id)
                    .map(|r| r.raw())
                    .unwrap_or(0),
                op_id.raw(),
                &root.to_string_lossy(),
                changed,
                &checks_ordered,
                &job_inputs,
                Some(enqueue_fingerprint),
            );
            if begin.is_err() {
                let err = begin.err().unwrap();
                tracing::error!(
                    "session {}: background verification attempt could not persist: {err}",
                    handle.id()
                );
                for (check, _) in &job_defs {
                    unavailable.push((check.id.clone(), check.command.clone()));
                }
                job_defs.clear();
                ordered_checks.clear();
                inline_outcomes.clear();
            } else {
                // The attempt is durable; its jobs never settle in this
                // turn. The gate records the mid-flight state: Pending,
                // criteria seeded as usual, task state Verifying.
                for (id, command) in &unavailable {
                    // Infallible verdict function: mirror row recorded
                    // (marker + audit) rather than failing the turn.
                    self.dw_note_upsert_memory_fact(
                        handle,
                        FactSource::Durable,
                        "verification",
                        id,
                        &format!("unavailable:{command}"),
                        DW_SITE_BACKGROUND_FACT,
                    );
                }
                let pending_acceptance = faktor_verify::acceptance(&checks, &results);
                let pending_status = match pending_acceptance {
                    faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
                    faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
                    faktor_verify::Acceptance::Pending => VerificationStatus::Pending,
                };
                self.persist_gate_facts(
                    handle,
                    &CompletionGate::VerificationPending,
                    pending_status,
                    &results,
                    changed,
                    criteria.as_deref(),
                );
                return TurnEndVerdict {
                    verification: results.clone(),
                    acceptance: Some(pending_acceptance),
                    review,
                    completion: Some(CompletionGate::VerificationPending),
                    criteria: criteria.clone(),
                    proof: None,
                };
            }
        }
        let acceptance = faktor_verify::acceptance(&checks, &results);
        if acceptance == faktor_verify::Acceptance::Fail {
            for check in checks.iter().filter(|c| c.required) {
                if results.iter().any(|(id, ok)| id == &check.id && !ok) {
                    // Durable fact: kind "verification", key = check id,
                    // value carries the failed command. Infallible verdict
                    // function: recorded (marker + audit), never propagated.
                    self.dw_note_upsert_memory_fact(
                        handle,
                        FactSource::Durable,
                        "verification",
                        &check.id,
                        &format!("failed:{}", check.command),
                        DW_SITE_CHECK_FAILED_FACT,
                    );
                }
            }
        }
        for (id, command) in &unavailable {
            // Durable row: the required check exists but could not run
            // (recorded, not propagated: this verdict function is infallible).
            self.dw_note_upsert_memory_fact(
                handle,
                FactSource::Durable,
                "verification",
                id,
                &format!("unavailable:{}", command),
                DW_SITE_CHECK_UNAVAILABLE_FACT,
            );
        }
        let failed: Vec<OutcomeReason> = checks
            .iter()
            .filter(|c| c.required)
            .filter(|c| results.iter().any(|(id, ok)| id == &c.id && !ok))
            .map(|c| {
                OutcomeReason::new(
                    ReasonCode::CheckFailed,
                    format!("required check '{}' ({}) failed", c.id, c.command),
                )
            })
            .collect();
        let completion = if !failed.is_empty() {
            CompletionGate::FailedVerification { reasons: failed }
        } else {
            let mut reasons: Vec<OutcomeReason> = unavailable
                .iter()
                .map(|(id, _)| {
                    OutcomeReason::new(
                        ReasonCode::CheckUnavailable,
                        format!("required check '{id}' unavailable"),
                    )
                })
                .collect();
            reasons.extend(review_blocking_reasons(review.as_ref(), quality));
            if reasons.is_empty() {
                CompletionGate::VerifiedComplete
            } else {
                CompletionGate::BlockedVerification { reasons }
            }
        };
        let status = match acceptance {
            faktor_verify::Acceptance::Fail => VerificationStatus::Failed,
            faktor_verify::Acceptance::Pass => VerificationStatus::Passed,
            faktor_verify::Acceptance::Pending => VerificationStatus::Pending,
        };
        // Per-attempt durable-proof payload (audit P0-8): assembled HERE,
        // where the workspace handle is open, so the changed-file evidence
        // digests the same repo state the checks ran against. Attempts whose
        // required checks produced NO verdict (only unavailable ones) carry
        // no proof — there is nothing a record could certify.
        // Audit item 3: only ADMITTED review evidence may back the durable
        // proof (a compaction summary or an ephemeral output can never be
        // admitted); the raw review value still gates through
        // `review_blocking_reasons` above.
        let review_evidence = admit_review_evidence(review.as_ref());
        let (proof, completion) = if executed.is_empty() {
            (None, completion)
        } else {
            match verification_proof_from_attempt(
                criteria.as_deref(),
                &checks,
                &results,
                &unavailable,
                &executed,
                changed,
                &ws,
                review_evidence.as_ref(),
            )
            .await
            {
                Ok(proof) => (Some(proof), completion),
                // Identity law (audit P1): a root with no provable content
                // identity cannot be minted into a durable proof — the
                // completion is blocked, never silently verified.
                Err(detail) => {
                    let mut reasons = match completion {
                        CompletionGate::BlockedVerification { reasons } => reasons,
                        _ => Vec::new(),
                    };
                    reasons.push(OutcomeReason::new(
                        ReasonCode::CheckUnavailable,
                        format!("durable verification proof refused: {detail}"),
                    ));
                    (None, CompletionGate::BlockedVerification { reasons })
                }
            }
        };
        self.persist_gate_facts(
            handle,
            &completion,
            status,
            &results,
            changed,
            criteria.as_deref(),
        );
        TurnEndVerdict {
            verification: results,
            acceptance: Some(acceptance),
            review,
            completion: Some(completion),
            criteria,
            proof,
        }
    }
}
