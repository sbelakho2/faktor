//! `runtime::turn::drive_call`: the provider-call phase of the logical turn
//! (the `WaitingForModel`/`Streaming`/settlement bodies mechanically moved
//! out of [`AgentRuntime::drive_turn_inner`], same statements and order).
//!
//! @authority-collapse: authority-bearing

use super::*;
use futures::StreamExt;

/// Outcome of `turn_model_call_begin`. The large variant is the one-shot
/// dispatch handle set (request + accounting machines); the enum is created
/// once per physical attempt and never stored, so the size difference is
/// deliberate indirection-free.
#[allow(clippy::large_enum_variant)]
pub(crate) enum CallBegin {
    /// The attempt is reserved, debited and dispatched-ready.
    Start(CallHandles),
    /// The call was refused before dispatch; the drive ends here.
    End,
}

/// The per-attempt handles `turn_model_call_begin` hands to the stream and
/// settlement helpers.
pub(crate) struct CallHandles {
    pub(crate) request: GenericAgentRequest,
    pub(crate) route_json: Option<String>,
    pub(crate) reservation: faktor_session::ReservationId,
    pub(crate) attempt_identity: ModelCallAttempt,
    pub(crate) acct: crate::AttemptAccounting,
    pub(crate) debits: crate::credits::AttemptDebits,
}

/// Outcome of one physical stream attempt.
pub(crate) enum AttemptFlow {
    /// The stream ended cleanly: settle this attempt.
    Settled,
    /// A retry-safe pre-content failure: run the next physical attempt.
    Retry,
    /// The call ended the drive (cancel, terminal failure, stall verdict).
    End,
}

impl AgentRuntime {
    /// Phase: the provider call (`WaitingForModel` into `Streaming`): the
    /// state-aware retry loop, per-attempt settlement and the post-call
    /// telemetry/stall checks.
    pub(crate) async fn turn_phase_provider_call(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
    ) -> faktor_core::Result<PhaseFlow> {
        // ---- provider call (state-aware retry, spec §13): a request
        // that failed BEFORE any content became durable may retry under
        // the retry policy (network class, bounded backoff). Once a tool
        // ran or assistant content was flushed, never replay.
        // Phase boundary: the drive is reasoning (paid provider call).
        self.note_execution_phase(st.handle, ExecutionPhase::Reasoning);
        st.handle
            .append_journal_event(
                faktor_core::event::EventKind::ModelStarted,
                AgentState::WaitingForModel,
                Some(st.op_id),
                None,
            )
            .await?;
        // Attempt-accounting audit: the policy is read ONCE per logical
        // call (an override installed mid-flight never changes the retry
        // shape of an in-flight turn).
        let retry_policy = self.effective_retry_policy();
        let max_attempts = retry_policy.max_attempts.max(1);
        st.iteration_progress = false;
        st.assistant_message = None;
        st.text_buf = String::new();
        st.reasoning_buf = String::new();
        st.tool_calls = Vec::new();
        st.tokens_in = 0;
        st.tokens_out = 0;
        // P0-1 settlement truth: the LAST usage frame's token categories
        // (uncached input / cache reads / cache writes / output —
        // reasoning billed at the output line) ride the settlement call,
        // so each reported category is priced at its OWN frozen
        // route-time line, never at a fabricated aggregate.
        st.frame_uncached_input = 0;
        st.frame_cache_read = 0;
        st.frame_cache_write = 0;
        st.frame_output = 0;
        // Telemetry basis of the logical call (P0-28): the final
        // attempt's latency + attempt count are recorded at the
        // terminal outcome sites after the loop.
        st.attempt_started = std::time::Instant::now();
        st.settled_attempt = 0;
        // The FINAL attempt's identity + reservation (attempt-accounting
        // audit): the attempt loop breaks with the settling attempt, so
        // its keyed row is written at the iteration's completed site
        // AFTER the loop from these captured values.
        st.settled_attempt_identity = None;
        st.settled_reservation = None;
        let wire_plan = st
            .wire_plan
            .take()
            .expect("route phase fixed the wire plan");
        'attempts: for attempt in 0..max_attempts {
            if attempt > 0 {
                // Bounded exponential backoff with jitter before the
                // next try (spec §13).
                let delay = retry_policy.next_delay(attempt - 1);
                tokio::time::sleep(delay).await;
            }
            let begin = self.turn_model_call_begin(st, &wire_plan, attempt).await?;
            let CallHandles {
                request,
                route_json,
                reservation,
                attempt_identity,
                mut acct,
                mut debits,
            } = match begin {
                CallBegin::Start(handles) => handles,
                CallBegin::End => return Ok(PhaseFlow::End),
            };
            match self
                .turn_stream_attempt(
                    st,
                    request,
                    attempt_identity,
                    reservation,
                    &mut acct,
                    &mut debits,
                    attempt,
                    max_attempts,
                    &retry_policy,
                )
                .await?
            {
                AttemptFlow::Settled => {}
                AttemptFlow::Retry => continue 'attempts,
                AttemptFlow::End => return Ok(PhaseFlow::End),
            }
            self.turn_attempt_settle(
                st,
                route_json,
                reservation,
                attempt_identity,
                &mut acct,
                &mut debits,
            )
            .await?;
            break 'attempts;
        }
        if let Some(mid) = st.assistant_message {
            if !st.reasoning_buf.is_empty() {
                st.handle
                    .append_reasoning_part(mid, &st.reasoning_buf)
                    .await?;
            }
            if !st.text_buf.is_empty() {
                st.handle.append_text_part(mid, &st.text_buf).await?;
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
            (st.settled_attempt_identity, st.settled_reservation)
        {
            st.handle.record_provider_call_attempt(
                attempt_identity,
                reservation_link(reservation),
                st.provider.id(),
                &st.model,
                "completed",
                Some(st.tokens_in),
                Some(st.tokens_out),
                None,
            )?;
        }
        // Production call receipt (F048/F090): ONE immutable normalized
        // requested-vs-effective record per settled physical attempt. The
        // sink is the daemon's foundation store; absent in tests/embedded.
        if let (Some(sink), Some(identity)) = (self.receipt_sink(), st.settled_attempt_identity) {
            let ended_ms = self.deps.clock.now_ms();
            let started_ms =
                ended_ms.saturating_sub(st.attempt_started.elapsed().as_millis() as i64);
            let mut requested = std::collections::BTreeMap::new();
            requested.insert(
                "provider".into(),
                serde_json::Value::String(st.provider.id().to_string()),
            );
            requested.insert("model".into(), serde_json::Value::String(st.model.clone()));
            let effective = requested.clone();
            sink.record(&crate::CallReceiptRecord {
                session_id: st.handle.id().raw(),
                op_id: identity.logical_op_id.raw().to_string(),
                call_id: format!("attempt-{}", identity.attempt_op_id.raw()),
                provider: st.provider.id().to_string(),
                model: st.model.clone(),
                route: format!("{}/{}", st.provider.id(), st.model),
                requested,
                effective,
                inherited: Vec::new(),
                started_ms,
                ended_ms: Some(ended_ms),
                status: "completed".into(),
                tokens_in: Some(st.tokens_in),
                tokens_out: Some(st.tokens_out),
                error: None,
            });
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
        let observation = wire_plan.prefix_observation(st.frame_cache_read);
        tracing::debug!(
            session = %st.handle.id(),
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
            st.handle.settle_usage_with_prefix_segments(
                st.op_id,
                st.provider.id(),
                &st.model,
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
            .reconcile_uncertain(st.handle.id(), st.task_id)
            .await
        {
            tracing::warn!(
                session = %st.handle.id(),
                task = %st.task_id,
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
            provider: st.provider.id().to_string(),
            model: st.model.clone(),
            phase: RouterPhase::Implement,
            success: true,
            retried: st.settled_attempt > 0,
            rate_limited: false,
            latency_ms: st
                .attempt_started
                .elapsed()
                .as_millis()
                .min(u64::MAX as u128) as u64,
            verified: None,
        };
        self.deps.routing.record_call_outcome(&settled_outcome);
        st.settled_calls
            .push((settled_outcome, st.drive_semantic_risk));

        // Stall signal (audit): several model iterations with NO new
        // durable state (no text/reasoning/tools) mean the agent is
        // buying tokens without progress — stop and re-plan instead.
        if st.detector.record_progress(st.iteration_progress, 8) {
            st.outcome.loop_stopped = true;
            st.outcome.stalled = true;
            // Classified end: the stall verdict must not be masked by a
            // journal error — record the lost Failed-journal.
            self.dw_note_journal_failed(
                st.handle,
                st.op_id,
                AgentState::FailedRecoverable,
                serde_json::json!({
                    "message": "stall detected: repeated model iterations produced no new state"
                }),
                DW_SITE_STALL_JOURNAL,
            )
            .await;
            st.outcome.final_state = AgentState::FailedRecoverable;
            return Ok(PhaseFlow::End);
        }
        // Expensive-cycle stall (P0-79): the iteration boundary is a
        // model-call boundary — each cycle costs tokens. The predicate
        // consults SEMANTIC EVIDENCE (output, evidence classes, op
        // completion), never bare tool events: an op whose iterations
        // only churn tools (heartbeats) stalls once no evidence class
        // arrived within the budget — a tool event does not mean the
        // task is closer to completion. Mid-stream silence is caught
        // by the stream watchdog above (pure-silence, unchanged).
        if self.progress_stalled_evidence(st.handle.id()) {
            st.outcome.loop_stopped = false;
            st.outcome.stalled = true;
            // Classified end: the evidence-stall verdict must not be
            // masked by a journal error — record the lost Failed-journal.
            self.dw_note_journal_failed(
                    st.handle,
                    st.op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({
                        "message": "stall detected: tool activity without semantic evidence within the silence budget"
                    }),
                    DW_SITE_STALL_EVIDENCE_JOURNAL,
                )
                .await;
            st.outcome.final_state = AgentState::FailedRecoverable;
            return Ok(PhaseFlow::End);
        }
        // Iteration completion is progress evidence.
        self.progress_heartbeat(st.handle.id());
        Ok(PhaseFlow::Continue)
    }

    /// Begin ONE physical model attempt: fresh attempt identity, request
    /// build + capability gates, durable budget reservation, commercial
    /// credit hold and the dispatch marker. `CallBegin::End` marks a typed
    /// pre-dispatch refusal already journaled on `st.outcome`.
    pub(crate) async fn turn_model_call_begin(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
        wire_plan: &WirePlan,
        attempt: u32,
    ) -> faktor_core::Result<CallBegin> {
        // Telemetry latency basis of this (final) attempt.
        st.attempt_started = std::time::Instant::now();
        st.settled_attempt = attempt;
        // Attempt identity (attempt-accounting audit): EVERY
        // physical retry is a NEW durable attempt with a fresh op
        // id and its OWN reservation; earlier uncertain attempts
        // keep their own reservations (they consume the parent task
        // budget until reconciled/finalized). Each attempt's
        // ATTEMPT-KEYED provider-call row is written ONCE at its
        // terminal site below (failed / completed) with this
        // identity.
        let attempt_identity =
            ModelCallAttempt::new(st.op_id, self.deps.session.try_next_op_id()?, attempt)
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::Internal,
                        "the attempt op id collided with the logical op id",
                    )
                })?;
        st.settled_attempt_identity = Some(attempt_identity);
        let request = self.build_request(
            st.handle, wire_plan, st.op_id, &st.model, &st.cancel, attempt,
        )?;
        CapabilityValidator::validate(&request, &st.caps)?;
        // The vision-like DOCUMENT gate at the wire boundary: a
        // provider that does not carry document parts can never
        // receive a resolved FileData part (defense in depth
        // behind the injection gate).
        CapabilityValidator::validate_documents(&request, st.provider.document_capable(&st.model))?;
        st.handle
            .append_journal_event(
                faktor_core::event::EventKind::ModelStarted,
                AgentState::Streaming,
                Some(st.op_id),
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
        st.provider_reported_cost = None;
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
        let predicted = match &st.routed_decision {
            Some(d) if d.estimated_cost_micro > 0 => est.max(d.estimated_cost_micro),
            _ => est,
        };
        let route_json = st
            .routed_decision
            .as_ref()
            .and_then(|d| serde_json::to_string(d).ok());
        let reservation = match self
            .deps
            .budgets
            .reserve_attempt(
                st.handle.id(),
                st.task_id,
                attempt_identity,
                predicted,
                // P0-1: the reserve freezes the route decision's
                // price capture on the reservation row (None = no
                // pricing authority — the unpriced pin/passthrough
                // paths; settlement then refuses under a hard cap
                // instead of fabricating an actual).
                st.routed_decision
                    .as_ref()
                    .and_then(|d| d.pricing_snapshot.clone()),
                // P1 classification: the route decision's durable execution
                // class rides the reservation beside its snapshot (Unknown
                // when no decision/authority exists — never a guessed local
                // or hosted class).
                st.routed_decision
                    .as_ref()
                    .map(|d| d.execution_class)
                    .unwrap_or(faktor_core::model::ExecutionClass::Unknown),
            )
            .await
        {
            Ok(r) => r,
            Err(SessionBudgetError::BudgetExceeded { .. }) => {
                st.outcome.final_state = AgentState::FailedRecoverable;
                let message = format!(
                            "budget exceeded: request would cost {predicted} micro of the remaining task budget"
                        );
                st.outcome.stop_reason = Some(OutcomeReason::new(
                    ReasonCode::BudgetExceeded,
                    message.clone(),
                ));
                // Classified end: the budget refusal must not be
                // masked by a journal error — record the lost append.
                self.dw_note_journal_failed(
                    st.handle,
                    st.op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({ "message": message }),
                    DW_SITE_BUDGET_JOURNAL,
                )
                .await;
                return Ok(CallBegin::End);
            }
            Err(e) => return Err(e.into()),
        };
        st.settled_reservation = Some(reservation);
        // The attempt's budget machine (attempt-accounting audit):
        // every terminal call of this attempt goes through the
        // guarded machine — refund only pre-dispatch, UNCERTAIN for
        // any post-dispatch failure, settle exactly once.
        let mut acct = crate::AttemptAccounting::new(
            self.deps.budgets.clone(),
            st.handle.id(),
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
            st.handle.id(),
            st.task_id,
            st.provider.id(),
            &st.model,
            attempt_identity,
            predicted,
        ) {
            Ok(machine) => machine,
            Err(e) => {
                if let Err(refund_err) = acct.fail_before_dispatch().await {
                    tracing::error!(
                        session = %st.handle.id(),
                        "refund of reservation {reservation} after a malformed debit identity failed: {refund_err}"
                    );
                }
                st.outcome.final_state = AgentState::FailedRecoverable;
                let message = format!("managed credit debit refused before dispatch: {e}");
                st.outcome.stop_reason.clone_from(&Some(OutcomeReason::new(
                    ReasonCode::BudgetExceeded,
                    message.clone(),
                )));
                // Classified end: record the lost Failed-journal
                // instead of masking the debit refusal with an error.
                self.dw_note_journal_failed(
                    st.handle,
                    st.op_id,
                    AgentState::FailedRecoverable,
                    serde_json::json!({ "message": message }),
                    DW_SITE_DEBIT_IDENTITY_JOURNAL,
                )
                .await;
                return Ok(CallBegin::End);
            }
        };
        if let Err(e) = debits.begin() {
            tracing::warn!(
                session = %st.handle.id(),
                attempt = %attempt_identity.attempt_op_id,
                "managed credit debit refused before dispatch: {e}"
            );
            // The provider was provably never contacted: release the
            // budget reservation (pre-dispatch refund) and stop.
            if let Err(refund_err) = acct.fail_before_dispatch().await {
                tracing::error!(
                    session = %st.handle.id(),
                    "refund of reservation {reservation} after a refused debit failed: {refund_err}"
                );
            }
            st.outcome.final_state = AgentState::FailedRecoverable;
            let message = format!("managed credit debit refused before dispatch: {e}");
            st.outcome.stop_reason = Some(OutcomeReason::new(
                ReasonCode::BudgetExceeded,
                message.clone(),
            ));
            // Classified end: record the lost Failed-journal instead
            // of masking the refusal with a journal error.
            self.dw_note_journal_failed(
                st.handle,
                st.op_id,
                AgentState::FailedRecoverable,
                serde_json::json!({ "message": message }),
                DW_SITE_DEBIT_REFUSED_JOURNAL,
            )
            .await;
            return Ok(CallBegin::End);
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
                session = %st.handle.id(),
                "cannot mark reservation {reservation} dispatched: {e}"
            );
            // The marker write failed BEFORE the request was sent:
            // the provider was provably never contacted — release
            // the reserved row (definitely-not-sent), then fail.
            if let Err(refund_err) = acct.fail_before_dispatch().await {
                tracing::error!(
                    session = %st.handle.id(),
                    "refund of the never-dispatched reservation {reservation} failed: {refund_err}"
                );
            }
            // Same pre-dispatch truth for the credit hold: the
            // provider was never called, so the hold is refunded.
            if let Err(refund_err) = debits.refund("dispatch_marker_failed") {
                tracing::error!(
                    session = %st.handle.id(),
                    "refund of the pre-dispatch credit hold failed: {refund_err}"
                );
            }
            return Err(e.into());
        }
        // The provider request is about to leave the process: from
        // here the credit hold may only settle or stay uncertain.
        debits.mark_dispatched();
        Ok(CallBegin::Start(CallHandles {
            request,
            route_json,
            reservation,
            attempt_identity,
            acct,
            debits,
        }))
    }

    /// Stream ONE physical attempt under the stall watchdog and settle its
    /// stream lifecycle; the buffer/tool/usage state rides `st`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn turn_stream_attempt(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
        request: GenericAgentRequest,
        attempt_identity: ModelCallAttempt,
        reservation: faktor_session::ReservationId,
        acct: &mut crate::AttemptAccounting,
        debits: &mut crate::credits::AttemptDebits,
        attempt: u32,
        max_attempts: u32,
        retry_policy: &faktor_core::retry::RetryPolicy,
    ) -> faktor_core::Result<AttemptFlow> {
        let mut stream = st.provider.stream(request);
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
                    if st.cancel.is_cancelled() {
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
                                session = %st.handle.id(),
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
                        if !st.text_buf.is_empty() || !st.reasoning_buf.is_empty() {
                            let mid = self
                                .ensure_assistant_message(st.handle, &mut st.assistant_message)
                                .await?;
                            if !st.text_buf.is_empty() {
                                st.handle.append_text_part(mid, &st.text_buf).await?;
                                st.text_buf.clear();
                            }
                            if !st.reasoning_buf.is_empty() {
                                st.handle.append_reasoning_part(mid, &st.reasoning_buf).await?;
                                st.reasoning_buf.clear();
                            }
                        }
                        // Cancel cleanup: recorded (marker + audit),
                        // never a different error for the Cancelled end.
                        self.dw_note_abort(st.handle, Some(st.op_id), DW_SITE_DRIVE_ABORT_DISPATCH);
                        st.outcome.final_state = AgentState::Cancelled;
                        return Ok(AttemptFlow::End);
                    }
                    match chunk {
                        Ok(ProviderChunk::Text { text }) => {
                            st.text_buf.push_str(&text);
                            let mid = self.ensure_assistant_message(st.handle, &mut st.assistant_message).await?;
                            // The chunk buffer is MOVED to the sink (no copy).
                            self.emit_chunk(st.handle.id(), Some(mid), "text", text);
                            st.iteration_progress = true;
                            // EPHEMERAL path: text deltas are NOT journaled per
                            // chunk (a multi-hour agent would commit millions of
                            // tiny SQLite events). The durable representation is
                            // the message part, flushed in bounded segments so a
                            // crash loses at most one segment.
                            if st.text_buf.len() >= STREAM_FLUSH_BYTES {
                                st.handle.append_text_part(mid, &st.text_buf).await?;
                                st.text_buf.clear();
                            }
                        }
                        Ok(ProviderChunk::Reasoning { text }) => {
                            st.reasoning_buf.push_str(&text);
                            let mid = self.ensure_assistant_message(st.handle, &mut st.assistant_message).await?;
                            // The chunk buffer is MOVED to the sink (no copy).
                            self.emit_chunk(st.handle.id(), Some(mid), "reasoning", text);
                            st.iteration_progress = true;
                            if st.reasoning_buf.len() >= STREAM_FLUSH_BYTES {
                                st.handle.append_reasoning_part(mid, &st.reasoning_buf).await?;
                                st.reasoning_buf.clear();
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
                            let mid = self.ensure_assistant_message(st.handle, &mut st.assistant_message).await?;
                            st.handle.append_tool_call_part(
                                mid,
                                &id,
                                &name,
                                input.clone(),
                                "completed",
                            ).await?;
                            self.emit_chunk(
                                st.handle.id(),
                                Some(mid),
                                "tool",
                                format!(
                                    "{name}\n{}",
                                    serde_json::to_string(&input).unwrap_or_default()
                                ),
                            );
                            st.tool_calls.push((id, name, input));
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
                            st.tokens_in = recorded_input;
                            st.tokens_out = usage.output_tokens;
                            // The LAST usage frame wins (providers
                            // settle once, usually at the end): its
                            // categories are the settlement basis
                            // (uncached input + the cache lines +
                            // output — reasoning already folds into
                            // the output line at the adapter, so
                            // the informational reasoning subset is
                            // never billed a second time).
                            st.frame_uncached_input = usage.uncached_input_tokens;
                            st.frame_cache_read = usage.cache_read_tokens;
                            st.frame_cache_write = usage.cache_write_tokens;
                            st.frame_output = usage.output_tokens;
                            if let Some(cost) = usage.reported_cost {
                                match authoritative_reported_micro(&cost) {
                                    Some(micro) => st.provider_reported_cost = Some(micro),
                                    None => {
                                        tracing::warn!(
                                            session = %st.handle.id(),
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
                                    session = %st.handle.id(),
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
                            st.handle.record_provider_call_attempt(
                                attempt_identity,
                                reservation_link(reservation),
                                st.provider.id(),
                                &st.model,
                                "failed",
                                None,
                                None,
                                Some(&e.to_string()),
                            )?;
                            // Retry ONLY when nothing durable happened in this
                            // request (no flushed parts, no message created, no
                            // tool runs pending) and the failure is retryable.
                            let safe = st.assistant_message.is_none()
                                && st.handle.pending_tool_runs()?.is_empty();
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
                                st.assistant_message = None;
                                st.text_buf.clear();
                                st.reasoning_buf.clear();
                                st.tool_calls.clear();
                                return Ok(AttemptFlow::Retry);
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
                                    provider: st.provider.id().to_string(),
                                    model: st.model.clone(),
                                    phase: RouterPhase::Implement,
                                    success: false,
                                    retried: st.settled_attempt > 0,
                                    rate_limited: matches!(
                                        e.kind,
                                        ProviderErrorKind::RateLimited
                                    ),
                                    latency_ms: st.attempt_started
                                        .elapsed()
                                        .as_millis()
                                        .min(u64::MAX as u128) as u64,
                                    verified: None,
                                },
                            );
                            self.handle_provider_failure(st.handle, st.op_id, e, &mut st.outcome).await?;
                            return Ok(AttemptFlow::End);
                        }
                    }
                }
                _ = stall_ticks.tick() => {
                    if self.progress_stalled(st.handle.id()) {
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
                        st.outcome.loop_stopped = false;
                        st.outcome.stalled = true;
                        // POST-dispatch stall verdict: the request
                        // left the process and the provider may have
                        // billed — UNCERTAIN, never a refund.
                        if let Err(uncertain_err) = acct
                            .fail_after_dispatch("stall_verdict", None)
                            .await
                        {
                            tracing::error!(
                                session = %st.handle.id(),
                                "cannot mark the stalled attempt uncertain: {uncertain_err}"
                            );
                        }
                        // Post-dispatch stall: the credit hold stays
                        // durable (the provider may have billed).
                        debits.close_uncertain();
                        self.handle_provider_failure(st.handle, st.op_id, err, &mut st.outcome)
                            .await?;
                        return Ok(AttemptFlow::End);
                    }
                }
            }
        }
        Ok(AttemptFlow::Settled)
    }

    /// Settle ONE completed attempt through the budget machine and follow
    /// the reservation ledger's settlement truth for the credit hold.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn turn_attempt_settle(
        self: &Arc<Self>,
        st: &mut TurnDriveState<'_>,
        route_json: Option<String>,
        reservation: faktor_session::ReservationId,
        attempt_identity: ModelCallAttempt,
        acct: &mut crate::AttemptAccounting,
        debits: &mut crate::credits::AttemptDebits,
    ) -> faktor_core::Result<()> {
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
                st.frame_uncached_input,
                st.frame_cache_read,
                st.frame_cache_write,
                st.frame_output,
                st.provider_reported_cost,
                route_json,
            )
            .await
        {
            Ok(settled) => settled,
            Err(settle_err) => {
                tracing::warn!(
                    session = %st.handle.id(),
                    "reservation {reservation} settlement refused: {settle_err}; marking the attempt uncertain"
                );
                if let Err(uncertain_err) = acct.fail_after_dispatch("settle_refused", None).await {
                    tracing::error!(
                        session = %st.handle.id(),
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
                        session = %st.handle.id(),
                        attempt = %attempt_identity.attempt_op_id,
                        "settlement of the managed credit hold at {actual_micro} micro failed (the hold stays durable for reconciliation): {e}"
                    );
                }
            }
            None => debits.close_uncertain(),
        }
        Ok(())
    }
}
