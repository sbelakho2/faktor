//! @authority-collapse: authority-bearing
//!
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
    /// never enters the request and the handoff is NEVER submitted as user
    /// text: it travels as a typed internal-context item rendered only
    /// under the Faktor-controlled `[faktor:internal-context]` section
    /// (untrusted DATA, never instructions). The render is bounded to
    /// `budget_tokens` before anything is submitted; an envelope that
    /// cannot fit is the typed Oversized error and nothing is submitted.
    /// Everything omitted stays retrievable through the handoff's scoped
    /// refs.
    pub async fn run_turn_from_handoff(
        self: &Arc<Self>,
        session: SessionId,
        handoff: &ChildHandoff,
        budget_tokens: usize,
    ) -> faktor_core::Result<TurnOutcome> {
        let input = ParentTurnInput::internal_only(InternalContextItem::ChildHandoff {
            provenance: "orchestrator:child-handoff".to_string(),
            handoff: handoff.clone(),
        });
        self.run_turn_input(session, input, budget_tokens).await
    }

    /// Run one parent logical turn from a STRUCTURED [`ParentTurnInput`]:
    /// the optional user message is the only text with user authority and
    /// is submitted byte-identically through the normal prompt path; typed
    /// internal context items are rendered ONLY by the Faktor-controlled
    /// internal-context section renderer and ride the turn's system slot as
    /// untrusted DATA. They never enter the user-role conversation. A
    /// render that cannot fit `budget_tokens` (irreducible envelope) fails
    /// typed BEFORE anything is submitted.
    pub async fn run_turn_input(
        self: &Arc<Self>,
        session: SessionId,
        input: ParentTurnInput,
        budget_tokens: usize,
    ) -> faktor_core::Result<TurnOutcome> {
        let internal = input
            .render_internal_bounded(budget_tokens)
            .map_err(|error| Error::oversized(error.to_string()))?;
        let ParentTurnInput { user_message, .. } = input;
        let prompt = user_message.unwrap_or_default();
        self.run_turn_structured(session, &prompt, &[], None, internal)
            .await
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
        self.run_turn_structured(session, prompt, files, model, None)
            .await
    }

    /// The ONE submission tail shared by every turn entry: submit the user
    /// prompt (user authority), attach the ALREADY-RENDERED Faktor internal
    /// context to the submitted operation when one exists, and drive it.
    async fn run_turn_structured(
        self: &Arc<Self>,
        session: SessionId,
        prompt: &str,
        files: &[String],
        model: Option<String>,
        internal: Option<String>,
    ) -> faktor_core::Result<TurnOutcome> {
        let receipt = self.submit(session, prompt, files)?;
        if let Some(rendered) = internal {
            self.store_internal_context(receipt.op_id, rendered);
        }
        if receipt.queued {
            // A single per-session turn runner delivers queued prompts after
            // the active logical turn completes (audit round 6). Never start
            // a second concurrent turn. The internal-context block stays
            // keyed by the queued operation id and is consumed by the drive
            // that admits it.
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
        // The per-phase bodies live in `drive_phases.rs` / `drive_call.rs`;
        // this skeleton is the phase order of the logical-turn loop, a
        // mechanical move of the single-function original (same statements,
        // same order, same semantics).
        let mut st = TurnDriveState::new(self, handle, op_id, cancel, model_override).await?;
        loop {
            match self.turn_phase_boundary(&mut st).await? {
                PhaseFlow::Continue => {}
                PhaseFlow::End => return Ok(st.outcome),
            }
            match self.turn_phase_context(&mut st).await? {
                PhaseFlow::Continue => {}
                PhaseFlow::End => return Ok(st.outcome),
            }
            match self.turn_phase_route(&mut st).await? {
                PhaseFlow::Continue => {}
                PhaseFlow::End => return Ok(st.outcome),
            }
            match self.turn_phase_provider_call(&mut st).await? {
                PhaseFlow::Continue => {}
                PhaseFlow::End => return Ok(st.outcome),
            }
            match self.turn_phase_tools_and_end(&mut st).await? {
                PhaseFlow::Continue => {}
                PhaseFlow::End => return Ok(st.outcome),
            }
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
        task_id: TaskId,
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
            task_id,
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
        task_id: TaskId,
        outcome: &mut TurnOutcome,
        ledger: &mut TaskLedger,
        turn_summary: &faktor_context::ledger::TurnSummary,
        settled_calls: &mut Vec<(SettledCallOutcome, u8)>,
        cancel: &CancellationToken,
    ) -> faktor_core::Result<()> {
        // Settlement is bound to the task identity this drive captured at
        // its start. A new chat prompt can start as soon as the TurnCompleted
        // journal event below is visible, while this tail is still folding
        // the row and applying the completion gate; re-reading the session's
        // (now advanced) task id would settle the OLD turn into the NEXT
        // task row.
        let settlement_task_id = task_id;
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
        // Engineering contract (audit item 4): a high/unknown risk contract
        // raises the end-of-turn verification depth to Strict — its risk
        // hypotheses demand the deeper review bar before completion.
        let quality = if self.contract_policy(handle)?.quality_strict {
            VerificationQuality::Strict
        } else {
            quality
        };
        let verdict = self
            .run_turn_verification(
                handle,
                op_id,
                settlement_task_id,
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
        let synced = self.sync_task_row(
            handle,
            settlement_task_id,
            ledger,
            verdict.criteria.as_deref(),
        )?;
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
            match handle.enforce_change_budget(settlement_task_id, &observations) {
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
        // Economic-truth P1 (dual producers, ONE durable truth): the
        // terminal-turn facts (risk class + explicit verified samples) are
        // persisted into the durable `terminal_run_facts` outbox BEFORE the
        // task row can reach a terminal state, so BOTH producers of the
        // run's engineering-outcome record — this completion gate and the
        // task executor's settlement — derive ONE byte-identical row from
        // durable facts. Facts that will never be consumed (a refusal below)
        // are corrected right after the gate lands; a persistence failure
        // only skips THIS site's record attempt: the settled calls keep the
        // per-call verified feed and the next settlement retries the record
        // with the facts that exist.
        let pre_terminal = self
            .session_task_bound(handle, settlement_task_id)?
            .is_some_and(|task| task.state.is_terminal());
        let mut terminal_facts: Option<TerminalRunFacts> = None;
        if !pre_terminal
            && !settled_calls.is_empty()
            && matches!(gate, Some(CompletionGate::VerifiedComplete))
        {
            let mut samples = Vec::new();
            let mut risk: Option<RiskBucket> = None;
            for (settled, risk_byte) in settled_calls.iter() {
                let bucket = risk_bucket_of(*risk_byte);
                risk = Some(bucket);
                samples.push(RunVerifiedSample {
                    provider: settled.provider.clone(),
                    model: settled.model.clone(),
                    phase: settled.phase,
                    verified_success: true,
                    rework_cost_micro: 0,
                    rework_turns: 0,
                });
            }
            samples.truncate(MAX_RUN_VERIFIED_SAMPLES);
            let facts = TerminalRunFacts {
                schema_version: TERMINAL_FACTS_SCHEMA_V,
                risk_class: risk,
                samples,
            };
            match TerminalRunFactsOutbox::new(self.deps.session.store())
                .persist(&session_terminal_run_id(handle.id()), &facts)
            {
                Ok(()) => terminal_facts = Some(facts),
                Err(error) => {
                    tracing::error!(
                        session = %handle.id(),
                        %error,
                        "persisting the terminal-run facts before the completion gate failed; \
                         this site skips the record and the next settlement retries"
                    );
                }
            }
        }
        let gate_landed = self.apply_gate_to_task_row_bound(
            handle,
            settlement_task_id,
            gate.clone(),
            verdict.proof.as_ref(),
        )?;
        if let (Some(facts), Some(_refusal)) = (&terminal_facts, &gate_landed) {
            // The completion claim was refused AFTER the facts were
            // persisted: keep the terminal-turn facts TRUTHFUL — this
            // turn's calls did not verify. A later completing turn
            // overwrites them (allowed until the record exists).
            let corrected = TerminalRunFacts {
                schema_version: TERMINAL_FACTS_SCHEMA_V,
                risk_class: facts.risk_class,
                samples: facts
                    .samples
                    .iter()
                    .map(|sample| RunVerifiedSample {
                        verified_success: false,
                        ..sample.clone()
                    })
                    .collect(),
            };
            if let Err(error) = TerminalRunFactsOutbox::new(self.deps.session.store())
                .persist(&session_terminal_run_id(handle.id()), &corrected)
            {
                tracing::error!(
                    session = %handle.id(),
                    %error,
                    "correcting the refused terminal-run facts failed; a later settlement may \
                     read the pre-refusal facts"
                );
            }
        }
        if gate_landed.is_none() && matches!(gate, Some(CompletionGate::VerifiedComplete)) {
            // P0-2 task-completion backstop: the task is terminal, so every
            // outstanding UNCERTAIN reservation (a crashed daemon may have
            // dispatched it and the provider may have billed) settles
            // conservatively AT its reserved estimate — the honest bound the
            // ledger already committed to. Idempotent; best-effort (a
            // finalize failure never rewinds the completion).
            let task_id = settlement_task_id;
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
            // Build this turn's attributed settled calls FIRST: when the
            // turn just completed the task, the run's ONE engineering-outcome
            // record carries exactly these samples (fed atomically by the
            // ledger append) instead of the per-call verified feed — a
            // sample is never fed twice and never dropped.
            let mut attributed: Vec<SettledCallOutcome> = Vec::with_capacity(settled_calls.len());
            for (settled, risk) in settled_calls.drain(..) {
                let bucket = risk_bucket_of(risk);
                let mut call = settled;
                call.verified = Some(VerifiedCallAttribution {
                    // The runtime has no per-task class dimension (every
                    // task is a goal-driven coding task on this graph); the
                    // class key stays the honest Medium default.
                    task_class: TaskClass::Medium,
                    risk_bucket: bucket,
                    verified_success: matches!(gate, CompletionGate::VerifiedComplete),
                    rework_cost_micro: 0,
                    rework_turns: 0,
                });
                attributed.push(call);
            }
            let feed_per_call = |call: &SettledCallOutcome| {
                self.deps.routing.record_call_outcome(call);
            };
            let feed_telemetry_only = |call: &SettledCallOutcome| {
                // The verified sample already rides the ledger record's
                // transaction; the telemetry record still lands per call.
                let mut telemetry = call.clone();
                telemetry.verified = None;
                self.deps.routing.record_call_outcome(&telemetry);
            };
            // A run whose task just reached its terminal state records the
            // ONE durable engineering-outcome row HERE (idempotent when an
            // earlier settlement already recorded it). The record is derived
            // from durable facts ONLY — including the terminal-turn facts
            // persisted BEFORE the terminal transition — so the executor's
            // concurrent settlement produces the byte-identical row.
            let terminal = self
                .session_task_bound(handle, settlement_task_id)?
                .is_some_and(|task| task.state == TaskState::VerifiedComplete);
            // Single feed: the durable record's atomic append already
            // absorbed ITS samples (ours, or a concurrent winner's), so only
            // the samples the durable record does NOT carry may still be fed
            // per call. Resolving this per sample — never all-or-nothing —
            // is what keeps a LOSING append (Conflict: the winner already
            // absorbed the samples) from double-counting the router stats.
            let mut absorbed: Vec<RunVerifiedSample> = Vec::new();
            if terminal && !attributed.is_empty() {
                if let Some(facts) = &terminal_facts {
                    let run_id = session_terminal_run_id(handle.id());
                    let ledger = OutcomeLedger::new(self.deps.session.store());
                    let attempt = ledger.record_terminal_run(handle, &run_id);
                    if let Err(error) = &attempt {
                        match error {
                            crate::outcome_ledger::OutcomeLedgerError::Store(
                                faktor_store::StoreError::Conflict(_),
                            ) => {
                                tracing::warn!(
                                    session = %handle.id(),
                                    %error,
                                    "a concurrent producer won the terminal outcome append; the \
                                     durable record stays authoritative and its samples are fed \
                                     at most once"
                                );
                            }
                            _ => {
                                tracing::error!(
                                    session = %handle.id(),
                                    %error,
                                    "terminal run outcome record failed; the settled calls keep \
                                     their per-call verified feed and the next settlement retries \
                                     the record"
                                );
                            }
                        }
                    }
                    absorbed = resolve_absorbed_samples(&ledger, &run_id, attempt, facts);
                }
                // `None`: the facts were never persisted for this terminal
                // transition (or the task was terminal before this turn):
                // never record from call-site state; the settled calls keep
                // their per-call verified feed and a later settlement retries
                // the record.
            }
            for call in attributed.iter() {
                if take_absorbed_sample(&mut absorbed, call, gate) {
                    feed_telemetry_only(call);
                } else {
                    feed_per_call(call);
                }
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
    // The verification tail needs the bound task identity in addition to
    // the turn inputs; grouping them in a struct only to satisfy the lint
    // would obscure the call site.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turn_verification(
        &self,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        bound_task: TaskId,
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
            // The run's bound task identity (never the session's current
            // identity, which may belong to a newer prompt).
            let task_id = bound_task.raw();
            {
                match handle.open_verification_jobs(task_id) {
                    Ok(open) if !open.is_empty() => {
                        if changed.is_empty() {
                            return self
                                .settle_verification_jobs(handle, bound_task, cancel)
                                .await;
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
                            let non_terminal = match handle.get_task(bound_task) {
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
                                return self
                                    .settle_verification_jobs(handle, bound_task, cancel)
                                    .await;
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
                return self.unverified_verdict(handle, changed, None, "session row unresolvable");
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
                );
            }
        };
        let ws = match self.open_effective_workspace(handle) {
            Some(w) => w,
            None => {
                return self.unverified_verdict(
                    handle,
                    changed,
                    None,
                    "workspace could not be opened for verification",
                );
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
                    service.policy().unit_max()
                }
                BudgetDecision::RunAsTaskOwnedOperation => service.policy().unit_max(),
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
                    budget_ms: service.policy().unit_max().as_millis() as u64,
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
        let (proof, mut completion) = if executed.is_empty() {
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
        // Engineering contract (autonomous-engineering spec + audit items
        // 2-4): evaluate the attempt's CLAIM-BOUND evidence against the
        // durable proof obligations at the CURRENT candidate snapshot. A
        // DISPROVEN obligation (or a violated constraint) blocks a passing
        // gate with its typed reason; the snapshot law stales prior support
        // whose proof basis did not survive; unsupported-but-not-disproven
        // states are persisted and disclosed, never silently passed. An
        // unevaluable contract never lets a passing gate stand (authority
        // law). A counterexample or invalidated assumption records a durable
        // replan decision.
        let candidate_snapshot =
            root_snapshot_best_effort(&ws).map(faktor_core::SubjectSnapshot::new);
        // The completion spine (audit item 6): when the daemon installed a
        // neutral verification registry, unresolved contract claims are
        // dispatched through it under this turn's op identity/cancellation.
        let verification_registry = self.verification_registry();
        let provider_dispatch = verification_registry.as_ref().map(|registry| {
            crate::runtime::contract_eval::ContractProviderDispatch {
                registry: registry.as_ref(),
                op_id,
                cancellation: cancel.clone(),
            }
        });
        let contract_outcome = crate::runtime::contract_eval::evaluate_and_persist_contract(
            handle,
            &executed,
            review.as_ref(),
            candidate_snapshot.as_ref(),
            provider_dispatch.as_ref(),
        )
        .await;
        if let Some(detail) = crate::runtime::contract_eval::replan_detail(&contract_outcome) {
            self.dw_note_ledger_decision(
                handle,
                "replan",
                "counterexample invalidates the assumption; re-open the work",
                &detail,
                DW_SITE_CONTRACT_REPLAN_DECISION,
            );
        }
        if let Some(detail) =
            crate::runtime::contract_eval::needs_verification_detail(&contract_outcome)
        {
            self.dw_note_ledger_decision(
                handle,
                "completion gate",
                "refuse VerifiedComplete",
                &detail,
                DW_SITE_CONTRACT_UNRESOLVED_DECISION,
            );
        }
        completion =
            crate::runtime::contract_eval::apply_contract_outcome(completion, contract_outcome);
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

    /// The engineering contract's verification policy for the session's
    /// CURRENT durable task row (audit item 4): high/unknown risk raises the
    /// depth, the end-of-turn quality and the routing quality floor; an
    /// unresolved required proof escalates intelligence/retrieval. FALLIBLE:
    /// a store read failure refuses the turn rather than silently skipping a
    /// high-risk floor. No task row = the empty contract = neutral policy.
    pub(crate) fn contract_policy(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<crate::runtime::contract_eval::ContractVerificationPolicy> {
        let contract = self
            .session_task(handle)?
            .map(|task| task.contract)
            .unwrap_or_default();
        Ok(crate::runtime::contract_eval::contract_verification_policy(
            &contract,
        ))
    }
}

/// Bounded attempts of the durable-record re-read that resolves absorbed
/// samples. A transient store failure must not silently collapse into an
/// empty absorbed set (which would re-feed every sample per call and
/// double-count the router's `model_outcome_stats`).
const ABSORBED_SAMPLES_READ_ATTEMPTS: usize = 3;

/// The samples a durable engineering record has ALREADY absorbed after one
/// `record_terminal_run` attempt (the single-feed rule): the record's
/// `verified_samples` when a record exists AND its provider/model
/// attribution is `complete` — ours (`Ok(Some(_))`), a pre-existing one
/// (`Ok(None)`), or a CONCURRENT winner's
/// ([`Conflict`](faktor_store::StoreError::Conflict), whose atomic append
/// absorbed ITS samples) — and nothing when the attempt failed for any
/// other reason. An attribution-INCOMPLETE record (the bounded provider-call
/// read truncated) absorbs no samples at all: its samples keep the per-call
/// verified feed — never dropped, never doubled. The durable read is retried
/// a bounded number of times; if it STILL fails the fallback is
/// NON-double-counting: both producers derive the record's samples from the
/// SAME frozen `terminal_run_facts` outbox, so the current facts' samples ARE
/// what the winner's append absorbed and are reported as absorbed rather
/// than re-fed per call (a persistently unreadable record can at worst
/// postpone coverage, never count a sample twice).
pub(super) fn resolve_absorbed_samples(
    ledger: &OutcomeLedger,
    run_id: &str,
    attempt: Result<
        Option<crate::outcome_ledger::LedgerAppend>,
        crate::outcome_ledger::OutcomeLedgerError,
    >,
    facts: &TerminalRunFacts,
) -> Vec<RunVerifiedSample> {
    let durable_samples = || -> Option<Vec<RunVerifiedSample>> {
        for attempt in 1..=ABSORBED_SAMPLES_READ_ATTEMPTS {
            match ledger.get(run_id) {
                Ok(Some(record)) => {
                    // The attribution gate mirrors `router_increments`: a
                    // record whose provider/model attribution is not
                    // COMPLETE fed NOTHING atomically, so its samples must
                    // stay on the per-call feed.
                    return Some(
                        if record.attribution_state == crate::AttributionState::Complete {
                            record.verified_samples
                        } else {
                            Vec::new()
                        },
                    );
                }
                Ok(None) => return Some(Vec::new()),
                Err(error) => {
                    tracing::warn!(
                        run_id,
                        attempt,
                        attempts = ABSORBED_SAMPLES_READ_ATTEMPTS,
                        %error,
                        "durable outcome record read failed while reconciling absorbed samples; \
                         retrying"
                    );
                }
            }
        }
        None
    };
    match attempt {
        // The append (or its byte-identical replay) carried exactly the
        // facts' samples when the record's attribution is complete; an
        // attribution-incomplete record absorbed none (see above), so the
        // durable read decides.
        Ok(Some(_)) => durable_samples().unwrap_or_else(|| facts.samples.clone()),
        // The record pre-existed: its durable samples are the truth; a
        // persistently unreadable record falls back to the frozen facts.
        Ok(None) => durable_samples().unwrap_or_else(|| facts.samples.clone()),
        // A CONCURRENT producer won with a different row: its atomic append
        // already absorbed ITS samples, so the loser must resolve coverage
        // against the durable record rather than re-feeding per call.
        Err(crate::outcome_ledger::OutcomeLedgerError::Store(
            faktor_store::StoreError::Conflict(_),
        )) => durable_samples().unwrap_or_else(|| facts.samples.clone()),
        Err(_) => Vec::new(),
    }
}

/// The explicit verified sample one attributed settled call contributes to
/// the durable record (the terminal-facts build site shape: rework is not
/// attributable here, 0 = unmeasured).
fn attributed_sample(call: &SettledCallOutcome, gate: &CompletionGate) -> RunVerifiedSample {
    RunVerifiedSample {
        provider: call.provider.clone(),
        model: call.model.clone(),
        phase: call.phase,
        verified_success: matches!(gate, CompletionGate::VerifiedComplete),
        rework_cost_micro: 0,
        rework_turns: 0,
    }
}

/// Consume ONE absorbed sample matching `call` (multiset semantics: a record
/// may carry several byte-identical samples from the capped terminal turn)
/// and report whether the durable record already fed it. A call the record
/// did NOT absorb keeps the per-call feed — never dropped, never doubled.
pub(super) fn take_absorbed_sample(
    absorbed: &mut Vec<RunVerifiedSample>,
    call: &SettledCallOutcome,
    gate: &CompletionGate,
) -> bool {
    let sample = attributed_sample(call, gate);
    match absorbed.iter().position(|absorbed| *absorbed == sample) {
        Some(index) => {
            absorbed.swap_remove(index);
            true
        }
        None => false,
    }
}
