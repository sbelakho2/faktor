//! `runtime::retry`: cohesive slice of the agent runtime.

#![allow(unused_imports)]

use super::*;

impl AgentRuntime {
    /// Install an additive provider-call retry policy for this runtime
    /// (test/tuning seam; see [`AgentRuntime::retry_policy_override`]).
    /// Poison-tolerant: a poisoned slot degrades to the deps policy, never
    /// a wedged turn.
    pub fn set_retry_policy(&self, policy: faktor_core::retry::RetryPolicy) {
        let mut slot = self
            .retry_policy_override
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *slot = Some(policy);
    }

    /// The provider-call retry policy this runtime applies: the installed
    /// override when present, else `AgentDeps::retry_policy` verbatim.
    pub(crate) fn effective_retry_policy(&self) -> faktor_core::retry::RetryPolicy {
        self.retry_policy_override
            .lock()
            .map(|slot| slot.unwrap_or(self.deps.retry_policy))
            .unwrap_or(self.deps.retry_policy)
    }

    /// Override the stall-silence budget (0 disables time-stall verdicts;
    /// tests inject small budgets instead of waiting out the default).
    pub fn set_stall_silence_ms(&self, ms: u64) {
        self.stall_silence_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn stall_silence(&self) -> u64 {
        self.stall_silence_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Drop a session's progress record (bounded: only live sessions hold
    /// records).
    pub(crate) fn drop_progress(&self, session: SessionId) {
        self.progress.lock().unwrap().remove(&session);
    }

    /// Feed: an operation for this session began.
    pub(crate) fn progress_begin_op(&self, session: SessionId, op_id: OpId) {
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.begin_op(now, op_id.to_string()));
    }

    /// Feed: the in-flight operation ended (completion is progress).
    pub(crate) fn progress_end_op(&self, session: SessionId) {
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.end_op(now));
    }

    /// Feed: durable output reached the client.
    pub(crate) fn progress_output(&self, session: SessionId) {
        if self.stall_silence() == 0 {
            return;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.output(now));
    }

    /// Feed: heartbeat evidence (tool events, iteration completions).
    pub(crate) fn progress_heartbeat(&self, session: SessionId) {
        if self.stall_silence() == 0 {
            return;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.progress(now));
    }

    /// Feed: a semantic evidence class occurred (P0-79). Evidence ≠
    /// activity: tool events alone never feed this — only the six
    /// [`ProgressEvidence`] classes (criterion status changed, failure
    /// fingerprint changed, repo state changed, new evidence admitted,
    /// plan step completed, verification improved) count toward progress
    /// for expensive-cycle decisions. Feeds the same stamp as a heartbeat
    /// plus the bounded evidence ring consulted by
    /// [`StallTracker::check_evidence`].
    pub(crate) fn progress_evidence(&self, session: SessionId, evidence: ProgressEvidence) {
        if self.stall_silence() == 0 {
            return;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.note_evidence(now, evidence));
    }

    /// Feed: the per-session repo-state digest map observes a file a tool
    /// outcome mutated (P0-79 site e). True when the digest differs from
    /// the last digest seen for that path — the caller then treats the
    /// batch as a repo-state step for the loop fingerprints. The digest
    /// map is bounded (256 paths, LRU) and survives across logical turns
    /// (per-session tracker).
    pub(crate) fn progress_repo_digest(
        &self,
        session: SessionId,
        path: &str,
        digest: FileHash,
    ) -> bool {
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.note_repo_digest(now, path, digest))
    }

    /// Feed: one genuine end wrote the verification/criteria gate facts
    /// (P0-79 site a). Folds the per-check statuses into the bounded
    /// criteria map and notes [`ProgressEvidence::CriterionStatusChanged`].
    pub(crate) fn progress_gate_facts(&self, session: SessionId, results: &[(String, bool)]) {
        if self.stall_silence() == 0 {
            return;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| {
            t.note_criteria_statuses(results);
            t.note_evidence(now, ProgressEvidence::CriterionStatusChanged);
        });
    }

    /// Evaluate the stall predicate; true = the session is stalled (no
    /// output, no progress, no completed op within the silence budget and
    /// no in-flight work or the in-flight work itself stuck). Feeding
    /// evidence is the only way back.
    pub(crate) fn progress_stalled(&self, session: SessionId) -> bool {
        if self.stall_silence() == 0 {
            return false;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.check(now))
    }

    /// The EVIDENCE-aware stall predicate (P0-79), consulted at the
    /// expensive-cycle decision sites (each iteration costs a model call):
    /// an op whose only liveness is tool events/heartbeats is stalled once
    /// no semantic evidence class arrived within the silence budget, even
    /// when a tool fires every few hundred ms. Durable output still counts
    /// (a stream of user-visible chunks is never stalled mid-stream; the
    /// pure-silence watchdog keeps guarding the stream itself).
    pub(crate) fn progress_stalled_evidence(&self, session: SessionId) -> bool {
        if self.stall_silence() == 0 {
            return false;
        }
        let now = self.deps.clock.now_ms();
        self.with_tracker(session, |t| t.check_evidence(now))
    }

    /// Feed one state-fingerprint step into the turn's loop detector
    /// (P0-78): the session's current `(repo_state, failure_fingerprint,
    /// criterion_state)` hashes folded from the per-session tracker state.
    /// The runtime calls this at the tool-batch boundary whenever the repo
    /// state actually moved (a write with a new digest). Returns the typed
    /// loop code when a patch→revert→patch oscillation trips.
    pub(crate) fn note_fingerprint_step(
        &self,
        session: SessionId,
        detector: &mut LoopDetector,
        action_class: &'static str,
    ) -> Option<ReasonCode> {
        let (repo_state, failure_fp, criteria_state) = {
            let map = self.progress.lock().unwrap();
            let t = map.get(&session)?;
            (
                t.repo_state_hash(),
                t.failure_state_hash(),
                t.criteria_state_hash(),
            )
        };
        let step = Fingerprint::new(repo_state, failure_fp, criteria_state, action_class);
        detector.record_state_step(&step)
    }

    /// The bounded progress record of one session as JSON (runtime health),
    /// for the native projection. None when the session has no record.
    pub fn progress_view(&self, session: SessionId) -> Option<serde_json::Value> {
        let map = self.progress.lock().unwrap();
        let t = map.get(&session)?;
        let now = self.deps.clock.now_ms();
        let mut t2 = t.clone();
        let silence = t2.silence_ms(now);
        let stalled = t2.is_stalled() || silence > t2.threshold_ms();
        Some(serde_json::json!({
            "lastOutputAt": t2.last_output_at(),
            "lastProgressAt": t2.last_progress_at(),
            "lastOpCompletedAt": t2.last_op_completed_at(),
            "inFlightOp": t2.in_flight_op(),
            "silenceMs": silence,
            "stallThresholdMs": t2.threshold_ms(),
            "stalled": stalled,
        }))
    }

    /// A provider stream failure is state-aware: if a tool already ran, the
    /// turn is NOT replayed; the journal decides the continuation.
    pub(crate) async fn handle_provider_failure(
        self: &Arc<Self>,
        handle: &faktor_session::SessionHandle,
        op_id: OpId,
        e: ProviderError,
        outcome: &mut TurnOutcome,
    ) -> faktor_core::Result<TurnOutcome> {
        let pending = handle.pending_tool_runs()?;
        let state = if pending.is_empty() {
            AgentState::FailedRecoverable
        } else {
            // A tool ran: never replay. Mark unknown and require verification.
            for row in &pending {
                handle.set_tool_run_effect(row.op_id, EffectStatus::Unknown)?;
            }
            AgentState::NeedsUserInput
        };
        // The classified end (Unknown effects + verification required) must
        // survive as the report even if the journal write fails: recorded
        // (marker + audit), never masked into a different error.
        self.dw_note_journal_failed(
            handle,
            op_id,
            state,
            serde_json::json!({ "message": e.message }),
            DW_SITE_PROVIDER_FAILURE_JOURNAL,
        )
        .await;
        outcome.final_state = state;
        Ok(outcome.clone())
    }
}
