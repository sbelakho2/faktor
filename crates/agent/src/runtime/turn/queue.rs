//! `runtime::turn::queue`: cohesive slice of the turn module.

#![allow(unused_imports)]

use super::*;

impl AgentRuntime {
    /// The ONE bounded coordination notice of this child's context boundary
    /// (P2): `coordination: N unread; use board_read`, folded from the
    /// child's durable unread counts and memoized by the run-family board
    /// REVISION. Only orchestrated children are served; post bodies never
    /// enter the notice (the count is the only value), an unchanged revision
    /// never rescans nor changes the prompt bytes, and any read failure
    /// (absent root, oversize board scan, store error) is neutral — no
    /// notice, never a turn error.
    pub(crate) fn child_coordination_notice(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<Option<String>> {
        if handle.orchestrator_child_identity_get()?.is_none() {
            return Ok(None);
        }
        // The board (and its revision) lives in the run-family ROOT's
        // ledger; the child's own head does not carry it.
        let root_id = handle.board_id()?.root();
        let Some(root) = self.deps.session.get_session(root_id)? else {
            return Ok(None);
        };
        let revision = root.ledger_ensure_head()?.board_revision;
        let child = faktor_session::board::ChildId::of_session(handle.id());
        let mut cache = self
            .coordination
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let memo = cache.entry(handle.id()).or_default();
        let mut read_error: Option<String> = None;
        let rendered = memo
            .notice_for(revision, || match handle.board_unread_counts(child) {
                Ok(counts) => counts.values().map(|count| u64::from(*count)).sum(),
                Err(err) => {
                    read_error = Some(err.message);
                    0
                }
            })
            .map(str::to_string);
        drop(cache);
        if let Some(message) = read_error {
            tracing::warn!(
                session = %handle.id(),
                board_revision = revision,
                error = %message,
                "coordination unread scan failed; no notice this boundary (neutral)"
            );
            return Ok(None);
        }
        Ok(rendered)
    }

    /// Resolve ONE durable permission request. `Allow` journals
    /// `PermissionGranted` and moves the machine to `ExecutingTool`; `Deny`
    /// journals `PermissionDenied`. A resolution unparks the logical turn a
    /// queue runner timed out on (WaitingForPermission is not continuable, so
    /// the runner's bounded wait expires and releases the gate): this path
    /// therefore RE-KICKS the session's durable queue when a non-terminal
    /// head is still waiting, so the queued prompts resume without a new
    /// submit or restart. The kick is a bounded runner (its own wait budget);
    /// a read error is never treated as an empty queue (the kick happens, or
    /// the failure is logged), and without an async runtime the durable head
    /// stays pending for the next kick/recovery.
    pub fn resolve_permission(
        self: &Arc<Self>,
        session: SessionId,
        permission_id: i64,
        decision: PermissionDecision,
    ) -> faktor_core::Result<()> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        handle.resolve_permission(permission_id, decision)?;
        // A live in-process driver owns the active turn (it handed the
        // decision to `handle.resolve_permission` itself): nothing is
        // parked, no kick needed. The kick is for the timed-out/resumed
        // shape.
        let live_driver = handle
            .active_turn_record()
            .ok()
            .flatten()
            .is_some_and(|record| handle.turn_cancellation(record.turn_op_id).is_some());
        if live_driver {
            return Ok(());
        }
        let pending = match handle.queued_prompt_count() {
            Ok(pending) => pending,
            Err(e) => {
                tracing::error!(
                    session = %session,
                    error = %e.message,
                    "permission resolved but the durable queue head is unreadable; kicking anyway \
                     (a read error is never an empty queue): {e}"
                );
                1
            }
        };
        if pending == 0 {
            return Ok(());
        }
        match tokio::runtime::Handle::try_current() {
            Ok(_) => {
                tracing::info!(
                    session = %session,
                    "permission resolved with a durable queue head pending; kicking the queue runner"
                );
                let runner = self.clone();
                tokio::spawn(async move { runner.run_session_queue(session).await });
            }
            Err(_) => tracing::warn!(
                session = %session,
                "permission resolved with a durable queue head pending but no async runtime is \
                 available; the durable head waits for the next kick/recovery"
            ),
        }
        Ok(())
    }

    pub fn abort(&self, session: SessionId) -> faktor_core::Result<Vec<OpId>> {
        self.abort_op(session, None)
    }

    /// Abort one operation (the active turn, a queued prompt, or a tool) or
    /// everything with `None`. Queued-prompt kills durably cancel their
    /// queue row without touching the machine; turn kills land the session
    /// ReadyForNextTurn (review P0-2).
    pub fn abort_op(
        &self,
        session: SessionId,
        op_id: Option<OpId>,
    ) -> faktor_core::Result<Vec<OpId>> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        Ok(handle.abort(op_id)?.op_ids)
    }

    /// Explicitly close a session (the only normal route to terminal
    /// closure; review P0-2 — Stop/abort cancels the turn, not the session).
    /// Commandment 8 (zero orphans): every child process owned by the
    /// session dies here — the supervisor kills the whole session process
    /// set (SIGTERM → grace → SIGKILL) BEFORE the durable end transition.
    pub fn end_session(&self, session: SessionId) -> faktor_core::Result<()> {
        let handle = self
            .deps
            .session
            .get_session(session)?
            .ok_or_else(|| Error::not_found(format!("session {session}")))?;
        if let Some(supervisor) = &self.deps.supervisor {
            let killed = supervisor.kill_all_for(faktor_terminal::ProcessOwner::Session(session));
            if !killed.is_empty() {
                tracing::info!(
                    "end_session: killed {} child process(es) of session {session}",
                    killed.len()
                );
            }
        }
        // Session lifecycle hook (audit): fired AFTER the session's children
        // are dead (zero-orphan ordering) and BEFORE the durable end
        // transition. Best-effort — it never blocks the close.
        self.run_lifecycle_hook(faktor_hooks::HookEvent::SessionEnd, session);
        // Idle unload (spec §21): the workspace watcher and the evidence
        // index are heavyweight per-workspace resources; a closed session
        // must not keep them alive forever.
        let row = handle.row()?;
        self.deps.workspaces.close(row.workspace_id);
        self.deps.evidence.forget(row.workspace_id);
        handle.end_session()?;
        // Bounded progress records: only live sessions keep them.
        self.drop_progress(session);
        Ok(())
    }

    /// The single per-session turn runner (audit round 6): waits for the
    /// active logical turn to finish, then delivers queued prompts one at a
    /// time as new logical turns (each with its own one-TurnCompleted flow).
    /// Exits when the queue is empty (or its bounded wait expires with the
    /// durable head still pending: the settle path/recovery re-kicks it).
    /// The per-session gate guarantees at most one runner per session.
    ///
    /// Boundary-race authority: a caller that arrives while a runner already
    /// owns the gate ARMS that runner for one more bounded pass instead of
    /// returning silently. The settle-path kick of a turn that just
    /// finished/cancelled (and every new queued submit) therefore can never
    /// be lost against a runner that is exiting exactly as its wait budget
    /// expires.
    pub async fn run_session_queue(self: &Arc<Self>, session: SessionId) {
        {
            let mut runners = self.runners.lock().unwrap();
            if let Some(gate) = runners.get_mut(&session) {
                // A live runner owns the session: arm exactly one more
                // bounded pass. It re-checks the durable head before
                // releasing the gate, so this request is never dropped.
                gate.rerun = true;
                return;
            }
            runners.insert(session, QueueRunnerGate::default());
        }
        loop {
            {
                let mut runners = self.runners.lock().unwrap();
                if let Some(gate) = runners.get_mut(&session) {
                    gate.passes += 1;
                }
            }
            let result = self.run_session_queue_inner(session).await;
            if let Err(e) = &result {
                tracing::warn!("queue runner for session {session} ended: {e}");
            }
            // The atomic gate handoff (boundary race): consume an armed pass
            // under the gate lock. An armed runner runs ONE more bounded pass
            // (the turn budget, never unbounded polling); otherwise the gate
            // is released only when the durable re-check saw no pending head
            // — which closes the start/exit window for a head that queued
            // (or was recovered) after the pass observed an empty queue. The
            // durable read happens BEFORE the lock: a kick racing it either
            // arms this gate (consumed under the lock) or finds the gate
            // already gone and runs its own loop.
            //
            // A store READ FAILURE is never an empty queue (audited): it is
            // loud, keeps the gate armed for a bounded number of extra
            // passes, records a durable retry marker, and only then — still
            // loudly — releases the gate so a broken store cannot wedge the
            // daemon forever. The durable rows stay pending for the next
            // kick/recovery; no prompt is ever lost to a transient error.
            let pending: faktor_core::Result<i64> = match self.deps.session.get_session(session) {
                Ok(Some(handle)) => handle.queued_prompt_count(),
                Ok(None) => Ok(0),
                Err(e) => Err(e),
            };
            #[cfg(test)]
            let pending = if durable_faults_tests::take(
                self.deps.session.store().root(),
                DW_SITE_QUEUE_HEAD_READ,
            ) {
                Err(Error::new(
                    ErrorKind::Store,
                    "injected durable queue-head read failure",
                ))
            } else {
                pending
            };
            // Decide under the gate lock; NOTHING awaits while the lock is
            // held (the backoff of a read-failure retry runs after release).
            enum GateDecision {
                AnotherPass,
                Release,
                Backoff(u32, faktor_core::Error),
                Exhausted(u32, faktor_core::Error),
            }
            let decision = {
                let mut runners = self.runners.lock().unwrap();
                let Some(gate) = runners.get_mut(&session) else {
                    return; // only this task removes its own gate
                };
                match pending {
                    Ok(pending) => {
                        gate.pending_read_failures = 0;
                        if std::mem::take(&mut gate.rerun) {
                            GateDecision::AnotherPass
                        } else if result.is_ok() && pending > 0 {
                            // A prompt appeared between the pass's empty
                            // observation and this decision: drain it under
                            // the same gate (audit round 7's start/exit race
                            // close). A pending head after a TYPED timeout
                            // does not loop here — the settle path/recovery
                            // kick arms or replaces this runner (bounded by
                            // the next pass's budget, never a poll loop).
                            GateDecision::AnotherPass
                        } else {
                            runners.remove(&session);
                            GateDecision::Release
                        }
                    }
                    Err(e) => {
                        gate.pending_read_failures = gate.pending_read_failures.saturating_add(1);
                        let failures = gate.pending_read_failures;
                        if failures <= MAX_QUEUE_HEAD_READ_RETRIES {
                            // Keep the gate ARMED: the durable head is
                            // unknown, never "empty". One bounded extra pass
                            // after a short backoff; the gate counter bounds
                            // the retries.
                            gate.rerun = true;
                            GateDecision::Backoff(failures, e)
                        } else {
                            gate.pending_read_failures = 0;
                            GateDecision::Exhausted(failures, e)
                        }
                    }
                }
            };
            match decision {
                GateDecision::AnotherPass => continue,
                GateDecision::Release => return,
                GateDecision::Backoff(failures, e) => {
                    tracing::error!(
                        session = %session,
                        failures,
                        bound = MAX_QUEUE_HEAD_READ_RETRIES,
                        error = %e.message,
                        "queue runner could not re-check the durable queue head; \
                         treating it as NON-EMPTY and keeping the gate armed: {e}"
                    );
                    tokio::time::sleep(QUEUE_HEAD_READ_RETRY_DELAY).await;
                    continue;
                }
                GateDecision::Exhausted(failures, e) => {
                    self.note_queue_head_read_failure(session, failures, &e);
                    return;
                }
            }
        }
    }

    pub(crate) async fn run_session_queue_inner(
        self: &Arc<Self>,
        session: SessionId,
    ) -> faktor_core::Result<()> {
        // Wait budget shared by every CONSECUTIVE non-progressing wait below
        // (an interrupted turn that is not continuable yet, a declined
        // admission): a session that never becomes eligible cannot make the
        // runner poll forever. It is the configured turn budget (fallback
        // when the operator opted out with 0); every real progress resets it.
        // On exhaustion the runner returns a TYPED timeout and the durable
        // queue head stays pending for the next re-kick.
        let wait_deadline = {
            let handle = self
                .deps
                .session
                .get_session(session)?
                .ok_or_else(|| Error::not_found(format!("session {session}")))?;
            bounded_turn_wait(&handle, MAX_QUEUE_WAIT)
        };
        let mut wait_started: Option<Instant> = None;
        loop {
            let handle = self
                .deps
                .session
                .get_session(session)?
                .ok_or_else(|| Error::not_found(format!("session {session}")))?;
            // Claimed queue rows from a crashed admission crash back to
            // pending so the durable head is re-admitted (idempotent), and a
            // `running` row whose logical turn already ended is retired to
            // `done` (its terminal mark was lost to the crash; re-admitting
            // it would deliver the same prompt twice).
            handle.recover_queued_rows()?;
            // A mid-flight machine blocks admission: when no LIVE driver owns
            // the active logical turn (post-restart), the residue is an
            // interrupted turn — resume the SAME recorded turn (same op id,
            // recorded model/envelope) before delivering queued prompts.
            if handle.queued_prompt_count()? > 0 {
                if let Some(record) = handle.active_turn_record()? {
                    let state = handle.state()?;
                    if handle.turn_cancellation(record.turn_op_id).is_none() {
                        if state_is_op_active(state) {
                            match self.continue_record(&handle, &record).await {
                                Ok(outcome) => {
                                    // The active turn record's OWN queue row
                                    // (when the interrupted turn was an
                                    // admitted queued prompt) is consumed by
                                    // this resume: mark it terminal exactly
                                    // once — a `running`/`claimed`/`pending`
                                    // row owned by the record must never be
                                    // re-admitted on top of the resumed turn.
                                    self.consume_record_queue_row(&handle, &record, &outcome)?;
                                    wait_started = None; // progress: fresh budget
                                    continue;
                                }
                                Err(e) => {
                                    // Not continuable yet (e.g. a durable
                                    // permission waits on the user): back off and
                                    // retry — the durable head stays pending. The
                                    // retry is BOUNDED: the turn budget (or the
                                    // fallback) ends the wait with a typed timeout.
                                    tracing::warn!(
                                        session = %session,
                                        turn = %record.turn_op_id,
                                        "queue runner cannot continue interrupted turn: {e}"
                                    );
                                    let started = *wait_started.get_or_insert_with(Instant::now);
                                    let waited = started.elapsed();
                                    if waited >= wait_deadline {
                                        return Err(Error::timeout(format!(
                                            "queue runner of session {session} could not continue \
                                             interrupted turn {} for {wait_deadline:?} (waited \
                                             {waited:?}); the durable queue head stays pending and \
                                             the settle path or recovery re-kicks the runner: {e}",
                                            record.turn_op_id
                                        )));
                                    }
                                    tokio::time::sleep(
                                        Duration::from_millis(200).min(wait_deadline - waited),
                                    )
                                    .await;
                                    continue;
                                }
                            }
                        } else if let Some(queue_seq) = record.queue_seq {
                            // The record is still active but the machine is no
                            // longer op-active: the logical turn is over and
                            // the record's queue row is bookkeeping residue.
                            // Retire the row (never re-admit it); the stale
                            // record itself is swept by the next drive's
                            // `recover_session`.
                            handle.mark_queued_status(queue_seq, "done")?;
                            continue;
                        }
                    }
                }
            }
            // Atomic admission: the store claims the head and materializes
            // the user message in ONE transaction when the session is
            // eligible (audit round 7 — no submission can cut between claim
            // and admission).
            let Some(admitted) = handle.admit_next_queued()? else {
                // Admission declined: either the queue is empty (exit) or
                // the session is mid-turn (wait for the active logical turn
                // to end, then re-try — the durable head stays pending). The
                // wait is BOUNDED: the turn budget (or the fallback) ends it
                // with a typed timeout instead of polling forever.
                if handle.queued_prompt_count()? == 0 {
                    return Ok(());
                }
                let started = *wait_started.get_or_insert_with(Instant::now);
                let waited = started.elapsed();
                if waited >= wait_deadline {
                    return Err(Error::timeout(format!(
                        "queue runner of session {session} was declined admission for \
                         {wait_deadline:?} (waited {waited:?}) with {} prompt(s) still queued; \
                         the durable queue head stays pending and the settle path or recovery \
                         re-kicks the runner",
                        handle.queued_prompt_count()?
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100).min(wait_deadline - waited)).await;
                continue;
            };
            wait_started = None; // admitted: the wait (if any) is over
            handle
                .append_journal_event(
                    faktor_core::event::EventKind::PromptAdmitted,
                    AgentState::Preparing,
                    Some(admitted.op_id),
                    Some(serde_json::json!({
                        "queue_seq": admitted.queue_seq,
                        "message_seq": admitted.message_seq,
                    })),
                )
                .await?;
            let queue_seq = admitted.queue_seq;
            handle.mark_queued_status(queue_seq, "running")?;
            let outcome = self.drive_admitted(&handle, &admitted).await;
            let status = match &outcome {
                Ok(o) if o.final_state == AgentState::Cancelled => "cancelled",
                Ok(_) => "done",
                Err(_) => "cancelled",
            };
            handle.mark_queued_status(queue_seq, status)?;
            if matches!(outcome, Ok(o) if o.final_state == AgentState::Cancelled) {
                return Ok(());
            }
        }
    }

    /// Retire the queue row OWNED by an active turn record after the resumed
    /// turn reached its end. The row may be `running` (the crash hit the
    /// drive), `claimed` (crash between claim and the first drive) or
    /// `pending` (crash recovery returned it); in every case the resumed
    /// turn IS that row's delivery and the row must never be re-admitted on
    /// top of it — otherwise the same prompt would be delivered twice. A
    /// record without a queue row (an immediate prompt) consumes nothing.
    /// Idempotent: a terminal row is simply rewritten to its terminal state.
    pub(crate) fn consume_record_queue_row(
        &self,
        handle: &faktor_session::SessionHandle,
        record: &faktor_store::TurnRecordRow,
        outcome: &TurnOutcome,
    ) -> faktor_core::Result<()> {
        let Some(queue_seq) = record.queue_seq else {
            return Ok(());
        };
        let status = if outcome.final_state == AgentState::Cancelled {
            "cancelled"
        } else {
            "done"
        };
        handle.mark_queued_status(queue_seq, status)
    }

    /// The queue runner exhausted its bounded durable queue-head RE-CHECKS
    /// (store read errors): record the failure on the durable audit surface
    /// (a `CrashDetected` self-transition naming the site) and release the
    /// gate loudly. The durable queue rows stay untouched/pending, so the
    /// next kick or startup recovery drains them — a transient store error
    /// can never silently drop a queued prompt.
    pub(crate) fn note_queue_head_read_failure(
        &self,
        session: SessionId,
        failures: u32,
        err: &faktor_core::Error,
    ) {
        tracing::error!(
            session = %session,
            failures,
            error = %err.message,
            "queue runner exhausted its bounded queue-head re-checks; the durable rows stay \
             pending and the next kick/recovery drains them"
        );
        let Ok(Some(handle)) = self.deps.session.get_session(session) else {
            return;
        };
        let state = match handle.state() {
            Ok(state) => state,
            Err(e) => {
                tracing::error!(
                    session = %session,
                    "queue-head read failure audit skipped: session state unreadable: {e}"
                );
                return;
            }
        };
        let payload = serde_json::json!({
            "durable_write_failure": {
                "site": DW_SITE_QUEUE_HEAD_READ,
                "failures": failures,
                "error": truncate(&err.message, 1024),
            }
        });
        if let Err(e) = handle.force_append_event(
            faktor_core::event::EventKind::CrashDetected,
            state,
            None,
            Some(payload),
        ) {
            tracing::error!(
                session = %session,
                "queue-head read failure audit could not be journaled: {e}"
            );
        }
    }

    /// Agent Manager cards (spec §15): daemon-owned background agents.
    pub fn cards(&self) -> faktor_core::Result<Vec<AgentCard>> {
        let mut out = Vec::new();
        for row in self.deps.session.list_sessions(None)? {
            let status = match row.state()? {
                AgentState::Completed => "completed".into(),
                AgentState::FailedPermanent | AgentState::FailedRecoverable => "failed".into(),
                AgentState::NeedsUserInput => "needs-input".into(),
                AgentState::WaitingForPermission => "waiting".into(),
                AgentState::Idle | AgentState::Suspended => "waiting".into(),
                _ => "running".into(),
            };
            out.push(AgentCard {
                session_id: row.id(),
                title: row.title()?,
                status,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------ recovery

    /// Crash-recovery sweep over every session (daemon startup, spec §7).
    /// Runtime-level: rows are resolved with runtime knowledge —
    /// workspace-scoped postcondition verification for workspace writes,
    /// one-time migration of legacy absolute-path `VerifyHash` rows to
    /// proven-inside workspace-relative identities (unprovable containment
    /// becomes Unknown/NeedsUserInput), unknown-effect marking.
    /// Interrupted turns whose rows need ASYNC replay (idempotent tools with
    /// a stored ReplayDescriptor) keep their rows running and their machine
    /// continuable; the per-session queue runner (or `continue_turn`)
    /// re-executes them ONCE with the recorded turn identity. Idempotent:
    /// a second sweep finds nothing pending.
    pub fn recover(&self) -> faktor_core::Result<Vec<RecoveryReport>> {
        let mut reports = Vec::new();
        for h in self.deps.session.list_sessions(None)? {
            reports.push(self.recover_session(&h)?);
        }
        Ok(reports)
    }

    /// Compensation surface for callers outside this file that must preserve
    /// an already-decided outcome across a failed turn-record close (the
    /// orchestrator's refused-isolation admission): the close is recorded on
    /// the durable retry channel (marker + audit) and replayed by the next
    /// session open. Never a silent discard; never a rewritten outcome.
    pub fn note_turn_record_close(
        &self,
        handle: &faktor_session::SessionHandle,
        turn_op: OpId,
        status: &str,
    ) {
        self.dw_note_finish_turn_record(handle, turn_op, status, DW_SITE_EXECUTOR_REFUSAL_RECORD);
    }

    /// Resolve interrupted tool runs of one session. Returns the rows that
    /// need ASYNC replay (idempotent tools with a stored descriptor), left
    /// running on the SAME row — the replay is a new physical attempt of the
    /// same logical operation. Everything else is finished durably here:
    /// workspace writes verify their recorded FilePostcondition through the
    /// workspace service (never a hash of JSON-encoded args, never a hash of
    /// a raw pathname); legacy VerifyHash rows are migrated ONCE — the old
    /// path is only a containment claim, canonicalized against the session's
    /// durable root, converted to a workspace-relative postcondition and
    /// verified through the handle, or classified Unknown/NeedsUserInput
    /// when containment cannot be proven; MarkUnknown/Manual/None and
    /// legacy descriptor-less Idempotent rows are marked failed/unknown
    /// (never blindly re-run).
    ///
    /// Terminalization atomicity: every terminal row commits its `tool_run`
    /// status/effect and its `RecoveryApplied` event in ONE store transaction
    /// (`Store::finish_recovered_tool_run_and_event`), which re-verifies the
    /// session's landing state inside the transaction and refuses an unknown
    /// or already-terminal row typed. There is no later, separately
    /// crashable `journal_recovery_applied` append for a recovered row: a
    /// crash at any durability boundary leaves either neither (row still
    /// `running`, no event) or both. The batch's landing state is committed
    /// FIRST by the sweep's one lawful `CrashDetected` transition (the store
    /// command self-transitions at the state it verifies); a verification
    /// refusal or a wrong state propagates typed BEFORE that row's
    /// transaction, so a refused row is never left row-without-event.
    pub(crate) fn recover_session(
        &self,
        handle: &faktor_session::SessionHandle,
    ) -> faktor_core::Result<RecoveryReport> {
        let session_id = handle.id();
        // Retry-on-next-open FIRST: reconstruct any durable transition a
        // failed write lost, before this sweep (or any driver) observes the
        // session.
        self.replay_durable_write_failures(handle);
        let pending = handle.pending_tool_runs()?;
        let current = handle.state()?;
        let mut report = RecoveryReport {
            session_id,
            state: current,
            crashed_ops: Vec::new(),
            orphans: Vec::new(),
            interrupted_turn: false,
            contradiction: false,
            applied: false,
        };
        // A LIVE in-process driver owns the session's active logical turn
        // (registered cancellation token): nothing crashed — recovery must
        // not journal CrashDetected, touch the driver's running rows, or
        // answer the calls the driver is still resolving. Checked BEFORE the
        // transcript repair and the fast path (the driver's own batch
        // legitimately has calls that are not answered yet). Post-restart
        // there is no tracking, so crash residue is swept.
        if let Some(rec) = handle.active_turn_record()? {
            if handle.turn_cancellation(rec.turn_op_id).is_some() {
                return Ok(report);
            }
        }
        // Crash-resume transcript integrity (see
        // [`AgentRuntime::answer_dangling_tool_calls`]): with NO open run
        // rows nothing will replay (a replay answers its own call), so any
        // unanswered call in the durable transcript is residue of the
        // interrupted turn and is answered BEFORE the machine is driven
        // again — the next wire request never carries a dangling call. A
        // TERMINAL session accepts no further prompts, so its transcript is
        // never sent again and is left byte-identical.
        if pending.is_empty() && !current.is_terminal() {
            report.applied |= self.answer_dangling_tool_calls(handle)? > 0;
        }
        if pending.is_empty() && !state_is_op_active(current) {
            return Ok(report);
        }
        // A machine that is not op-active while rows are pending is a
        // journal/ledger contradiction the session sweep knows how to fix
        // (rows finished, state stands). Runtime-level finishing would
        // journal illegal transitions from Idle/Suspended/terminal states.
        // `FailedRecoverable` is the ONE exception: it is this sweep's own
        // failure landing state, so a restart that resumes a partially
        // terminalized failed batch re-enters the sweep (the per-row store
        // command re-verifies that state and lands the remaining rows there,
        // stickily) instead of delegating to a second classifier — that is
        // what makes every crash boundary converge to one identical durable
        // result.
        if !state_is_op_active(current) && current != AgentState::FailedRecoverable {
            return handle
                .recover_all()
                .map_err(|e| Error::new(ErrorKind::Store, format!("session recovery: {e}")));
        }
        report.applied = true;
        if pending.is_empty() {
            // Interrupted turn without tool rows (the crash hit the model
            // stream): the ONE durable fact is the CrashDetected annotation
            // at the CURRENT state (self-transition — the machine stays
            // continuable so the SAME logical turn resumes with its recorded
            // identity, never a crash_target hop that kills it); the runner /
            // continue_turn resumes the recorded turn.
            let last_kind = self.last_event_kind(handle);
            if last_kind != Some(faktor_core::event::EventKind::CrashDetected) {
                handle.append_event(
                    faktor_core::event::EventKind::CrashDetected,
                    current,
                    None,
                    Some(serde_json::json!({
                        "pending_ops": 0,
                        "recovered_from": state_tag(current),
                    })),
                )?;
            }
            report.interrupted_turn = true;
            report.state = current;
            return Ok(report);
        }

        // Classify every row BEFORE writing anything: the classification is
        // read-only (a malformed postcondition/recovery row aborts the whole
        // sweep before any durable write), and the batch shape (all-replayable
        // vs terminal) is only knowable with every verdict in hand.
        enum Verdict {
            Verify { postcondition: FilePostcondition },
            LegacyVerify { path: String, expected: FileHash },
            FailUnknown,
            DeferReplay,
        }
        let mut verdicts: Vec<(ToolRunRow, Verdict)> = Vec::with_capacity(pending.len());
        for row in pending {
            if let Some(pc) = row.postcondition.clone() {
                let pc: FilePostcondition = serde_json::from_value(pc).map_err(|e| {
                    Error::malformed(format!(
                        "tool_run {} carries a corrupt postcondition: {e}",
                        row.op_id
                    ))
                })?;
                verdicts.push((row, Verdict::Verify { postcondition: pc }));
                continue;
            }
            let recovery: RecoveryStrategy = serde_json::from_value(row.recovery.clone())
                .map_err(|e| Error::malformed(format!("corrupt recovery row: {e}")))?;
            match recovery {
                RecoveryStrategy::VerifyHash { path, expected } => {
                    verdicts.push((row, Verdict::LegacyVerify { path, expected }));
                }
                RecoveryStrategy::MarkUnknown
                | RecoveryStrategy::Manual
                | RecoveryStrategy::None => {
                    verdicts.push((row, Verdict::FailUnknown));
                }
                RecoveryStrategy::Idempotent => match row.replay_descriptor.as_ref() {
                    Some(desc) => {
                        // Validate the stored invocation BEFORE deferring:
                        // a hostile descriptor is a loud error, never a blind
                        // replay (validated again at replay time).
                        self.validate_replay_descriptor(&row, desc)?;
                        verdicts.push((row, Verdict::DeferReplay));
                    }
                    None => verdicts.push((row, Verdict::FailUnknown)),
                },
            }
        }
        // Resolution: an all-replayable batch keeps every row RUNNING on the
        // SAME row — the async replay is a new physical attempt of the same
        // logical operation, and the machine stays continuable (the
        // interrupted turn resumes with its recorded identity). This branch
        // makes no row transaction at all; `CrashDetected` at the CURRENT
        // state (self-transition) is its one journal fact.
        let all_deferrable = !verdicts.is_empty()
            && verdicts
                .iter()
                .all(|(_, v)| matches!(v, Verdict::DeferReplay));
        if all_deferrable {
            let last_kind = self.last_event_kind(handle);
            if last_kind != Some(faktor_core::event::EventKind::CrashDetected) {
                handle.append_event(
                    faktor_core::event::EventKind::CrashDetected,
                    current,
                    None,
                    Some(serde_json::json!({
                        "pending_ops": verdicts.len(),
                        "recovered_from": state_tag(current),
                    })),
                )?;
            }
            for (row, _) in &verdicts {
                report.crashed_ops.push(RecoveredOp {
                    op_id: row.op_id,
                    tool: row.tool.clone(),
                    status: "running".into(),
                    effect: EffectStatus::Unknown,
                    action: RecoveryAction::RerunAllowed,
                });
            }
            report.state = handle.state()?;
            return Ok(report);
        }

        // Terminal batch: EVERY terminal row and its `RecoveryApplied` event
        // commit in ONE store transaction ([`finish_recovered_row`]), which
        // re-verifies the session's landing state and refuses an unknown or
        // already-terminal row typed. The batch's one state move therefore
        // happens FIRST, via the lawful `CrashDetected` transition this sweep
        // appends: a crash after that transition and before a row
        // transaction leaves rows `running` (never a terminal row without
        // its event), and a restart re-enters this sweep from the landing
        // state. A refused verification propagates BEFORE its row's
        // transaction, so that row stays running too.
        let pending_ops = verdicts.len();
        let mut landing;
        for (row, verdict) in &verdicts {
            match verdict {
                Verdict::Verify { postcondition } => {
                    let actual = self.verify_workspace_file(handle.id(), postcondition)?;
                    if actual == Some(postcondition.expected_hash) {
                        landing =
                            self.land_recovery_batch(handle, AgentState::Validating, pending_ops)?;
                        self.finish_recovered_row(
                            handle,
                            row,
                            "completed",
                            EffectStatus::Verified,
                            "verified",
                            None,
                            landing,
                        )?;
                        report.crashed_ops.push(RecoveredOp {
                            op_id: row.op_id,
                            tool: row.tool.clone(),
                            status: "completed".into(),
                            effect: EffectStatus::Verified,
                            action: RecoveryAction::Verified {
                                expected: postcondition.expected_hash,
                                actual: actual.unwrap_or(postcondition.expected_hash),
                            },
                        });
                    } else {
                        // The file does not match the recorded postcondition:
                        // the write never landed (or was overwritten) — FAIL
                        // LOUDLY, never silently "applied".
                        landing = self.land_recovery_batch(
                            handle,
                            AgentState::FailedRecoverable,
                            pending_ops,
                        )?;
                        self.finish_recovered_row(
                            handle,
                            row,
                            "failed",
                            EffectStatus::Failed,
                            "not_applied",
                            None,
                            landing,
                        )?;
                        report.crashed_ops.push(RecoveredOp {
                            op_id: row.op_id,
                            tool: row.tool.clone(),
                            status: "failed".into(),
                            effect: EffectStatus::Failed,
                            action: RecoveryAction::NotApplied {
                                expected: postcondition.expected_hash,
                                actual,
                            },
                        });
                    }
                }
                Verdict::LegacyVerify { path, expected } => {
                    // One-time migration (audit P1-F): a legacy absolute
                    // pathname is NEVER an execution capability. Containment
                    // must be PROVEN against the session's durable workspace
                    // root; only then is the old path converted to a
                    // normalized relative path and recorded durably on the
                    // running row as the modern postcondition BEFORE any
                    // read. A row whose containment cannot be proven is
                    // classified Unknown/NeedsUserInput — never Verified, and
                    // no raw path is ever read.
                    let Some(postcondition) =
                        self.migrate_legacy_verify_row(handle, row, path, *expected)?
                    else {
                        landing = self.land_recovery_batch(
                            handle,
                            AgentState::FailedRecoverable,
                            pending_ops,
                        )?;
                        self.finish_recovered_row(
                            handle,
                            row,
                            "failed",
                            EffectStatus::Unknown,
                            "legacy_unverifiable",
                            None,
                            landing,
                        )?;
                        report.crashed_ops.push(RecoveredOp {
                            op_id: row.op_id,
                            tool: row.tool.clone(),
                            status: "failed".into(),
                            effect: EffectStatus::Unknown,
                            action: RecoveryAction::NeedsHuman,
                        });
                        continue;
                    };
                    let relative = postcondition.relative_path.clone();
                    let actual = self.verify_workspace_file(handle.id(), &postcondition)?;
                    if actual == Some(*expected) {
                        landing =
                            self.land_recovery_batch(handle, AgentState::Validating, pending_ops)?;
                        self.finish_recovered_row(
                            handle,
                            row,
                            "completed",
                            EffectStatus::Verified,
                            "verified",
                            Some(&relative),
                            landing,
                        )?;
                        report.crashed_ops.push(RecoveredOp {
                            op_id: row.op_id,
                            tool: row.tool.clone(),
                            status: "completed".into(),
                            effect: EffectStatus::Verified,
                            action: RecoveryAction::Verified {
                                expected: *expected,
                                actual: actual.unwrap_or(*expected),
                            },
                        });
                    } else {
                        landing = self.land_recovery_batch(
                            handle,
                            AgentState::FailedRecoverable,
                            pending_ops,
                        )?;
                        self.finish_recovered_row(
                            handle,
                            row,
                            "failed",
                            EffectStatus::Failed,
                            "not_applied",
                            Some(&relative),
                            landing,
                        )?;
                        report.crashed_ops.push(RecoveredOp {
                            op_id: row.op_id,
                            tool: row.tool.clone(),
                            status: "failed".into(),
                            effect: EffectStatus::Failed,
                            action: RecoveryAction::NotApplied {
                                expected: *expected,
                                actual,
                            },
                        });
                    }
                }
                Verdict::FailUnknown => {
                    landing = self.land_recovery_batch(
                        handle,
                        AgentState::FailedRecoverable,
                        pending_ops,
                    )?;
                    self.finish_recovered_row(
                        handle,
                        row,
                        "failed",
                        EffectStatus::Unknown,
                        "unknown_effect",
                        None,
                        landing,
                    )?;
                    report.crashed_ops.push(RecoveredOp {
                        op_id: row.op_id,
                        tool: row.tool.clone(),
                        status: "failed".into(),
                        effect: EffectStatus::Unknown,
                        action: RecoveryAction::UnknownEffect,
                    });
                }
                Verdict::DeferReplay => {
                    // A sibling ended the turn: this replayable row cannot
                    // rejoin it — failed honestly, never left running.
                    landing = self.land_recovery_batch(
                        handle,
                        AgentState::FailedRecoverable,
                        pending_ops,
                    )?;
                    self.finish_recovered_row(
                        handle,
                        row,
                        "failed",
                        EffectStatus::Unknown,
                        "unknown_effect",
                        None,
                        landing,
                    )?;
                    report.crashed_ops.push(RecoveredOp {
                        op_id: row.op_id,
                        tool: row.tool.clone(),
                        status: "failed".into(),
                        effect: EffectStatus::Unknown,
                        action: RecoveryAction::RerunAllowed,
                    });
                }
            }
        }
        // If any row resolved as a failure the machine landed
        // FailedRecoverable: the interrupted turn is over — close its record.
        if handle.state()? == AgentState::FailedRecoverable {
            if let Some(rec) = handle.active_turn_record()? {
                // Recovery sweep: failing here would abort the whole sweep on
                // one record close, so the loss is recorded, not propagated.
                self.dw_note_finish_turn_record(
                    handle,
                    rec.turn_op_id,
                    "failed",
                    DW_SITE_RECOVERY_RECORD,
                );
            }
        }
        report.state = handle.state()?;
        Ok(report)
    }

    /// Land the session on the state the transactional per-row recovery
    /// command verifies. `finish_recovered_tool_run_and_event` self-transitions
    /// at the state it verifies, so a terminal batch's ONE state move is a
    /// separate lawful `CrashDetected` transition committed BEFORE the row
    /// commands — the same shape the session sweep uses for its crash target
    /// (`crates/session/src/recovery.rs`). A crash after this transition and
    /// before a row command leaves rows `running`; a restart re-enters the
    /// sweep and re-verifies. `FailedRecoverable` is sticky: the machine has
    /// no edge back to `Validating`, so a resumed failed batch keeps landing
    /// its remaining rows there (`to` is ignored and the current state
    /// returned). Already being on `to` is a no-op.
    pub(crate) fn land_recovery_batch(
        &self,
        handle: &faktor_session::SessionHandle,
        to: AgentState,
        pending_ops: usize,
    ) -> faktor_core::Result<AgentState> {
        let current = handle.state()?;
        if current == to || current == AgentState::FailedRecoverable {
            return Ok(current);
        }
        handle.append_event(
            faktor_core::event::EventKind::CrashDetected,
            to,
            None,
            Some(serde_json::json!({
                "pending_ops": pending_ops,
                "recovered_from": state_tag(current),
            })),
        )?;
        Ok(to)
    }

    /// Finish ONE recovered tool run through the transactional recovery
    /// command: expected-state re-verify, the exactly-one-running-row terminal
    /// update, the gapless journal event and the session state commit in ONE
    /// store transaction. The event is the row's `RecoveryApplied`; there is
    /// no later, separately crashable append. A wrong expected state or an
    /// already-terminal row is the typed `Conflict` and leaves no trace (the
    /// store transaction rolls back whole).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_recovered_row(
        &self,
        handle: &faktor_session::SessionHandle,
        row: &ToolRunRow,
        status: &str,
        effect: EffectStatus,
        action: &str,
        legacy_migrated_to: Option<&str>,
        landing: AgentState,
    ) -> faktor_core::Result<()> {
        let mut payload = serde_json::json!({
            "op_id": row.op_id.raw(),
            "tool": row.tool,
            "status": status,
            "effect": effect_tag(effect),
            "action": action,
        });
        if let Some(relative) = legacy_migrated_to {
            // Durable evidence of the one-time legacy migration (audit
            // P1-F): the run was verified through this normalized
            // workspace-relative identity, never through the recorded raw
            // pathname.
            payload["legacy_migrated_to"] = serde_json::json!(relative);
        }
        self.deps
            .session
            .store()
            .finish_recovered_tool_run_and_event(
                handle.id(),
                row.op_id,
                status,
                effect_tag(effect),
                faktor_core::event::EventKind::RecoveryApplied,
                landing,
                Some(payload),
            )
            .map_err(|e| match e {
                faktor_store::StoreError::Conflict(msg) => Error::conflict(format!(
                    "recovery terminalization of tool run {} refused: {msg}",
                    row.op_id
                )),
                other => Error::new(
                    ErrorKind::Store,
                    format!(
                        "recovery terminalization of tool run {} failed: {other}",
                        row.op_id
                    ),
                ),
            })?;
        Ok(())
    }

    /// Continue one recorded interrupted logical turn (crash recovery):
    /// resolve side effects, replay deferred idempotent runs exactly once,
    /// walk the machine back to WaitingForModel, then drive the SAME
    /// recorded turn op with the SAME recorded model — never a synthesized
    /// op and never the session's current defaults.
    pub(crate) async fn continue_record(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        record: &faktor_store::TurnRecordRow,
    ) -> faktor_core::Result<TurnOutcome> {
        let turn_op = record.turn_op_id;
        if handle.turn_cancellation(turn_op).is_some() {
            return Err(Error::conflict(format!(
                "turn {turn_op} already has a live driver"
            )));
        }
        let state = handle.state()?;
        if !state_is_op_active(state) {
            return Err(Error::conflict(format!(
                "session {} is {:?}; no interrupted logical turn to continue",
                handle.id(),
                state
            )));
        }
        // Resolve side effects (existing tool-run recovery; idempotent runs
        // come back as deferred rows and are replayed below).
        self.recover_session(handle)?;
        let state = handle.state()?;
        // Replay deferred idempotent runs ONCE each (the row stays the SAME
        // logical operation; only the attempt counter moves).
        if state == AgentState::ExecutingTool {
            let pending = handle.pending_tool_runs()?;
            for row in &pending {
                self.replay_tool_run(handle, row).await?;
            }
        }
        let state = handle.state()?;
        match state {
            AgentState::FailedRecoverable
            | AgentState::FailedPermanent
            | AgentState::Cancelled
            | AgentState::Completed
            | AgentState::NeedsUserInput => {
                // The interrupted turn is over (its effects resolved as
                // failed/unknown): report the end, never re-drive it. The
                // close is recorded, not propagated: the outcome below is the
                // genuine report and must not be masked by a record failure.
                let status = if state == AgentState::Cancelled {
                    "cancelled"
                } else {
                    "failed"
                };
                self.dw_note_finish_turn_record(
                    handle,
                    turn_op,
                    status,
                    DW_SITE_CONTINUE_RECORD_DONE,
                );
                return Ok(TurnOutcome {
                    op_id: turn_op,
                    final_state: state,
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
                });
            }
            AgentState::WaitingForPermission | AgentState::ToolRequested => {
                return Err(Error::conflict(format!(
                    "session {} waits on a durable permission; resolve it before continuing",
                    handle.id()
                )));
            }
            _ => {}
        }
        self.walk_to_waiting(handle, turn_op)?;
        // Session lifecycle hook (audit): continue_record is the ONLY
        // recovery-resume boundary (an interrupted logical turn is re-driven
        // after a crash — via continue_turn or the queue runner). Fires only
        // once the turn is actually about to drive; fresh prompts that run
        // recover_session defensively never reach it. Best-effort.
        self.run_lifecycle_hook(faktor_hooks::HookEvent::SessionResume, handle.id());
        let outcome = self
            .drive_turn(
                handle,
                turn_op,
                CancellationToken::new(),
                Some(record.effective_model.clone()),
            )
            .await;
        if outcome.is_err() {
            // Failure path: the drive's original error is the report; the
            // record close is recorded, not propagated (marker + audit).
            self.dw_note_finish_turn_record(
                handle,
                turn_op,
                "failed",
                DW_SITE_CONTINUE_RECORD_FAILED,
            );
        }
        outcome
    }
}
