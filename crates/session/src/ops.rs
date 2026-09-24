//! Operation ledger: tool runs, provider calls, permission requests, and the
//! in-memory registry that gives the session ownership of every in-flight op's
//! cancellation token.

use std::collections::HashMap;
use std::sync::Mutex;

use faktor_core::cancellation::CancellationToken;
use faktor_core::capability::Capability;
use faktor_core::event::EventKind;
use faktor_core::id::OpId;
use faktor_core::op::{EffectStatus, ModelCallAttempt, OpMeta};
use faktor_core::state::AgentState;
use faktor_store::{CommandEvent, ToolRunRow};

use crate::handle::SessionHandle;
use crate::{effect_str, json_bytes, SessionError, MAX_TOOL_ARGS_BYTES};

/// What kind of operation an id refers to (drives abort's event kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// A user prompt turn.
    Turn,
    /// A tool execution.
    Tool,
}

#[derive(Debug, Clone)]
pub(crate) struct TrackedOp {
    pub kind: OpKind,
    pub token: CancellationToken,
}

/// In-memory ownership registry: every registered op holds its cancellation
/// token here so `abort` can fan cancellation out. Shared per session through
/// the manager (all handle clones cancel the same ops).
#[derive(Debug, Default)]
pub(crate) struct OpRegistry {
    inner: Mutex<HashMap<OpId, TrackedOp>>,
}

impl OpRegistry {
    pub fn register(&self, op: OpId, kind: OpKind, token: CancellationToken) {
        crate::recover_lock(&self.inner).insert(op, TrackedOp { kind, token });
    }

    pub fn register_turn(&self, op: OpId, token: CancellationToken) {
        self.register(op, OpKind::Turn, token);
    }

    pub fn unregister(&self, op: OpId) {
        crate::recover_lock(&self.inner).remove(&op);
    }

    pub fn tracked(&self, op: OpId) -> Option<TrackedOp> {
        crate::recover_lock(&self.inner).get(&op).cloned()
    }

    pub fn kind(&self, op: OpId) -> Option<OpKind> {
        self.tracked(op).map(|t| t.kind)
    }

    pub fn cancel(&self, op: OpId) {
        if let Some(t) = self.tracked(op) {
            t.token.cancel();
        }
    }

    pub fn all(&self) -> Vec<OpId> {
        crate::recover_lock(&self.inner).keys().copied().collect()
    }
}

/// Handle to a started tool run.
#[derive(Debug, Clone)]
pub struct ToolRunHandle {
    pub op_id: OpId,
    pub tool: String,
    pub row_id: i64,
    pub started_ms: i64,
}

/// A pending permission request awaiting the user.
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub id: i64,
    pub op_id: OpId,
    pub capability: Capability,
    pub event_seq: faktor_core::id::EventSeq,
    /// The durable deadline (`permission.expires_ms`). The live requester
    /// computes its remaining wait from this value — never from a fresh
    /// configured window — so a daemon restart cannot extend the deadline.
    pub expires_ms: i64,
}

fn capability_tag(cap: &Capability) -> String {
    serde_json::to_value(cap)
        .ok()
        .and_then(|v| {
            v.get("capability")
                .and_then(|t| t.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Build the journal half of an atomic session command. The typed payload
/// schema is decoded BEFORE any write, so an undecodable payload refuses the
/// whole command — never a side row without its journal event.
pub(crate) fn command_event(
    kind: EventKind,
    state: AgentState,
    op_id: Option<OpId>,
    ts_ms: i64,
    payload: Option<serde_json::Value>,
) -> Result<CommandEvent, SessionError> {
    crate::payload::decode_payload(kind, crate::payload::PAYLOAD_SCHEMA_V, payload.as_ref())?;
    Ok(CommandEvent {
        kind,
        state,
        op_id,
        ts_ms,
        payload,
        payload_ver: crate::payload::PAYLOAD_SCHEMA_V,
    })
}

impl SessionHandle {
    /// Start a tool run. The op envelope is the caller's (it owns the
    /// deadline/retry/cancellation/recovery); this command journals
    /// `ToolStarted` and registers the op so abort can cancel it. Requires the
    /// session to be in `WaitingForPermission`/`ToolRequested`/`ExecutingTool`
    /// (auto-allowed tools must still pass through the `ToolRequested` hop so
    /// the journal is complete).
    pub fn start_tool_run(
        &self,
        op: OpMeta,
        tool: &str,
        args: serde_json::Value,
    ) -> faktor_core::Result<ToolRunHandle> {
        if op.session_id != self.id {
            return Err(SessionError::NotFound(format!(
                "op {} belongs to session {}, not {}",
                op.operation_id, op.session_id, self.id
            ))
            .into());
        }
        op.ensure_alive(self.now_ms()).map_err(SessionError::from)?;
        if tool.is_empty() || tool.len() > 256 {
            return Err(SessionError::Malformed(format!("invalid tool name {tool:?}")).into());
        }
        if json_bytes(&args) > MAX_TOOL_ARGS_BYTES {
            return Err(SessionError::Oversized(format!(
                "tool args of {} bytes exceed MAX_TOOL_ARGS_BYTES",
                json_bytes(&args)
            ))
            .into());
        }
        let _guard = self.command_guard();
        let recovery = serde_json::to_value(&op.recovery)
            .map_err(|e| SessionError::Malformed(format!("recovery serialization: {e}")))?;
        let expected_hash = match &op.recovery {
            faktor_core::op::RecoveryStrategy::VerifyHash { expected, .. } => {
                Some(expected.to_hex())
            }
            _ => None,
        };
        // Validate the transition before any durable write, and read the
        // pre-state ONCE: the atomic store command re-verifies it inside the
        // transaction, so a concurrent transition refuses loudly instead of
        // leaving a tool_run row without its journal event.
        let current = self.state()?;
        crate::journal::validate_transition(
            current,
            EventKind::ToolStarted,
            AgentState::ExecutingTool,
        )?;
        let event = command_event(
            EventKind::ToolStarted,
            AgentState::ExecutingTool,
            Some(op.operation_id),
            self.now_ms(),
            Some(serde_json::json!({ "tool": tool })),
        )?;
        let (row_id, _seq) = self
            .manager
            .store()
            .start_tool_run_and_event(
                self.id,
                op.operation_id,
                tool,
                args,
                recovery,
                expected_hash,
                op.replay.clone(),
                current,
                event,
            )
            .map_err(crate::map_store_err)?;
        self.ops()
            .register(op.operation_id, OpKind::Tool, op.cancellation.clone());
        Ok(ToolRunHandle {
            op_id: op.operation_id,
            tool: tool.to_string(),
            row_id,
            started_ms: op.start_time_ms,
        })
    }

    /// Finish a tool run durably and journal the outcome. `completed` lands
    /// on `Validating`; `failed` on `FailedRecoverable`; `cancelled` on
    /// `Cancelled` (the session ends — cancel is terminal by the core
    /// machine). Finishing an unknown op is `NotFound` (loud).
    pub fn finish_tool_run(
        &self,
        op: OpId,
        status: &str,
        effect: EffectStatus,
    ) -> faktor_core::Result<()> {
        if !matches!(status, "completed" | "failed" | "cancelled") {
            return Err(SessionError::Malformed(format!("invalid tool status {status:?}")).into());
        }
        let _guard = self.command_guard();
        // Must exist before journaling anything.
        let pending = self.pending_tool_runs()?;
        if !pending.iter().any(|r| r.op_id == op) {
            return Err(SessionError::NotFound(format!("tool run {op} is not running")).into());
        }
        let (kind, state) = match status {
            "completed" => (EventKind::ToolCompleted, AgentState::Validating),
            "failed" => (EventKind::ToolCompleted, AgentState::FailedRecoverable),
            "cancelled" => (EventKind::ToolCancelled, AgentState::Cancelled),
            _ => unreachable!("validated above"),
        };
        // Validate against the pre-state read ONCE; the atomic store command
        // re-verifies it inside the same transaction as the row update.
        let current = self.state()?;
        crate::journal::validate_transition(current, kind, state)?;
        let event = command_event(
            kind,
            state,
            Some(op),
            self.now_ms(),
            Some(serde_json::json!({ "status": status, "effect": effect_str(effect) })),
        )?;
        self.manager
            .store()
            .finish_tool_run_and_event(self.id, op, status, effect_str(effect), current, event)
            .map_err(crate::map_store_err)?;
        self.ops().unregister(op);
        Ok(())
    }

    pub fn set_tool_run_effect(&self, op: OpId, effect: EffectStatus) -> faktor_core::Result<()> {
        self.manager
            .store()
            .set_tool_run_effect(self.id, op, effect_str(effect))
            .map_err(|e| crate::map_store_err(e).into())
    }

    pub fn pending_tool_runs(&self) -> faktor_core::Result<Vec<ToolRunRow>> {
        self.manager
            .store()
            .pending_tool_runs(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Record the workspace-write postcondition a tool reported at execution
    /// end (v7): crash recovery verifies the CURRENT file bytes against it
    /// through the workspace file service. Only a still-running row may be
    /// annotated (loud otherwise).
    pub fn record_tool_postcondition(
        &self,
        op: OpId,
        postcondition: &serde_json::Value,
    ) -> faktor_core::Result<()> {
        self.manager
            .store()
            .record_tool_postcondition(self.id, op, postcondition)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Bump the physical-attempt counter of one still-running tool run (a
    /// crash-recovery replay is a new physical attempt of the same logical
    /// operation). Returns the new attempt number.
    pub fn bump_tool_attempt(&self, op: OpId) -> faktor_core::Result<i64> {
        self.manager
            .store()
            .bump_tool_run_attempt(self.id, op)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Durably open the record of an admitted logical turn (v7). The runtime
    /// drives the turn with exactly this identity; recovery never synthesizes
    /// one. Re-admission of the same turn op upserts the same record; other
    /// active records of the session are finalized as failed (at most one
    /// active logical turn per session).
    #[allow(clippy::too_many_arguments)]
    pub fn start_turn_record(
        &self,
        turn_op: OpId,
        queue_seq: Option<i64>,
        prompt_message_id: Option<i64>,
        provider: &str,
        model: &str,
        variant: Option<&str>,
    ) -> faktor_core::Result<i64> {
        if provider.len() > 256 || model.len() > 256 {
            return Err(SessionError::Oversized("provider/model name too long".into()).into());
        }
        self.manager
            .store()
            .start_turn_record(
                self.id,
                turn_op,
                queue_seq,
                prompt_message_id,
                provider,
                model,
                variant,
            )
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Finalize the recorded envelope at logical-turn start: the effective
    /// provider/model (per-message override wins over the session default),
    /// the reasoning variant and the tool-call mode. Only an active record
    /// is updated.
    pub fn set_turn_envelope(
        &self,
        turn_op: OpId,
        provider: &str,
        model: &str,
        variant: Option<&str>,
        tool_mode: Option<&str>,
    ) -> faktor_core::Result<()> {
        self.manager
            .store()
            .set_turn_record_envelope(self.id, turn_op, provider, model, variant, tool_mode)
            .map_err(|e| crate::map_store_err(e).into())
            .map(|_| ())
    }

    /// Close an active turn record. Idempotent: absent or already-closed
    /// records are a no-op (returns whether anything was updated).
    pub fn finish_turn_record(&self, turn_op: OpId, status: &str) -> faktor_core::Result<bool> {
        self.manager
            .store()
            .finish_turn_record(self.id, turn_op, status)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// The session's single active logical-turn record (v7). `None` when no
    /// prompt was admitted as an active turn (e.g. everything is queued).
    pub fn active_turn_record(&self) -> faktor_core::Result<Option<faktor_store::TurnRecordRow>> {
        self.manager
            .store()
            .active_turn_record(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    pub fn turn_record(
        &self,
        turn_op: OpId,
    ) -> faktor_core::Result<Option<faktor_store::TurnRecordRow>> {
        self.manager
            .store()
            .turn_record_of(self.id, turn_op)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Every turn record of the session (oldest first; diagnostics/tests).
    pub fn turn_records(&self) -> faktor_core::Result<Vec<faktor_store::TurnRecordRow>> {
        self.manager
            .store()
            .turn_records_of(self.id)
            .map_err(|e| crate::map_store_err(e).into())
    }

    /// Record a provider wire call (never cancels the session; provider calls
    /// are sub-operations of a turn).
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call(
        &self,
        op: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
    ) -> faktor_core::Result<i64> {
        if provider.len() > 256 || model.len() > 256 {
            return Err(SessionError::Oversized("provider/model name too long".into()).into());
        }
        Ok(self
            .manager
            .store()
            .record_provider_call(
                self.id, op, provider, model, status, tokens_in, tokens_out, error,
            )
            .map_err(crate::map_store_err)?)
    }

    /// Async twin of [`SessionHandle::record_provider_call`] (usage
    /// settlement, audit 13/42): one provider usage frame lands in the
    /// durable `provider_call` rows through the manager's DbActor, and the
    /// caller awaits the fsynced response.
    #[allow(clippy::too_many_arguments)]
    pub async fn settle_usage(
        &self,
        op: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
    ) -> faktor_core::Result<i64> {
        if provider.len() > 256 || model.len() > 256 {
            return Err(SessionError::Oversized("provider/model name too long".into()).into());
        }
        let handle = self.manager.actor().handle();
        Ok(handle
            .settle_usage(
                self.id, op, provider, model, status, tokens_in, tokens_out, error,
            )
            .await
            .map_err(crate::map_store_err)?)
    }

    /// ATTEMPT-ORIENTED provider-call record (attempt accounting, schema
    /// v18): the row carries THIS physical attempt's fresh op id
    /// (`attempt_op_id` + ordinal) and its reservation id, next to the
    /// shared logical model-call op id (the row's `op_id` /
    /// `parent_model_call_op_id`), so budget reconciliation joins the
    /// attempt's crashed reservation to exactly THIS row — never to a
    /// sibling attempt's. `attempt` must pair a distinct `attempt_op_id`
    /// with its `logical_op_id` (a reused op id would key two attempts into
    /// one row: the audit hole this API closes). Legacy callers keep using
    /// [`SessionHandle::record_provider_call`]/[`SessionHandle::settle_usage`]
    /// with attempt-less rows.
    #[allow(clippy::too_many_arguments)]
    pub fn record_provider_call_attempt(
        &self,
        attempt: ModelCallAttempt,
        reservation: Option<crate::budget::ReservationId>,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
    ) -> faktor_core::Result<i64> {
        if provider.len() > 256 || model.len() > 256 {
            return Err(SessionError::Oversized("provider/model name too long".into()).into());
        }
        // Defensive re-validation (a deserialized attempt could pair the
        // same id twice): a physical attempt must never reuse its logical
        // parent's op id.
        if ModelCallAttempt::new(
            attempt.logical_op_id,
            attempt.attempt_op_id,
            attempt.ordinal,
        )
        .is_none()
        {
            return Err(SessionError::Malformed(format!(
                "attempt {} must differ from its logical op {} (a physical attempt gets a fresh op id)",
                attempt.attempt_op_id, attempt.logical_op_id
            ))
            .into());
        }
        // Reservation ids are AUTOINCREMENT row ids (>= 1): 0 would be a
        // fabricated link to nothing — reject it, never persist it.
        if reservation.is_some_and(|r| r.raw() == 0) {
            return Err(SessionError::Malformed(
                "reservation id 0 is not a durable reservation (ids start at 1)".into(),
            )
            .into());
        }
        Ok(self
            .manager
            .store()
            .record_provider_call_attempt(
                self.id,
                &attempt,
                reservation.map(|r| r.raw()),
                provider,
                model,
                status,
                tokens_in,
                tokens_out,
                error,
            )
            .map_err(crate::map_store_err)?)
    }

    /// Prefix-cache settlement twin (audits 65-66 fill site, architecture
    /// §8.4): the usage row of a completed provider call additionally
    /// records the byte-truth prefix observation the runtime measured —
    /// `prompt_prefix_hash` (digest of the exact cacheable-prefix bytes the
    /// sent request carried: StaticPrefix + SemiStable) and `prompt_tokens`
    /// (that prefix's estimated token count) — and the row's per-turn
    /// `prefix_stability` against the session's PREVIOUS observation
    /// (1.0 for the first observation and for byte-identical prefixes;
    /// prev/cur under append-consistent growth; 0.0 on a rewrite), mirroring
    /// the documented pair rule in `faktor-router::stability` that the
    /// routing layer applies over these durable rows.
    ///
    /// Unlike [`SessionHandle::settle_usage`] this twin is synchronous and
    /// writes DIRECTLY through the store: the DbActor's hot-write batch
    /// shape (`HotWrite::RecordProviderCall`) predates the v13 prefix
    /// columns, and the prefix fill happens once per completed call, not on
    /// a hot chunk path — durability semantics are identical (one
    /// transaction, fsynced). Prefix-less calls (both `None`) land exactly
    /// like a plain `record_provider_call` with the stability columns NULL.
    ///
    /// A row that carries a prefix hash but NO stability never exists: the
    /// stability is derived here from byte truth (previous observation
    /// hash/count), never accepted from callers, and corrupt previous rows
    /// fail loudly (`Malformed`) instead of silently mispairing.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_usage_with_prefix(
        &self,
        op: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
        prompt_prefix_hash: Option<[u8; 32]>,
        prompt_tokens: Option<u64>,
    ) -> faktor_core::Result<i64> {
        // Legacy shape: no per-call segment observation (the row's v19
        // column stays NULL, routing keeps the binary pair rule).
        self.settle_usage_with_prefix_segments(
            op,
            provider,
            model,
            status,
            tokens_in,
            tokens_out,
            error,
            prompt_prefix_hash,
            prompt_tokens,
            None,
        )
    }

    /// Additive v19 twin of [`SessionHandle::settle_usage_with_prefix`]:
    /// the prefix row additionally persists the raw per-call segment
    /// observation JSON (audit 45 `PrefixObservation`: ordered segment
    /// digests + token counts + observed cache reads) the runtime measured.
    /// `None` records the legacy NULL — absence is never guessed. The
    /// payload is validated LOUDLY by the store (bounded, strict shape,
    /// equal-length digest/token vectors) on write, and again on every read
    /// by [`Store::provider_call_prefix_rows`], so a corrupt row is a typed
    /// `Malformed` instead of a silently degraded observation.
    #[allow(clippy::too_many_arguments)]
    pub fn settle_usage_with_prefix_segments(
        &self,
        op: OpId,
        provider: &str,
        model: &str,
        status: &str,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<&str>,
        prompt_prefix_hash: Option<[u8; 32]>,
        prompt_tokens: Option<u64>,
        prefix_segments_json: Option<&str>,
    ) -> faktor_core::Result<i64> {
        if provider.len() > 256 || model.len() > 256 {
            return Err(SessionError::Oversized("provider/model name too long".into()).into());
        }
        let stability = match (prompt_prefix_hash, prompt_tokens) {
            (Some(hash), Some(tokens)) => {
                let prev = self.last_prefix_observation()?;
                Some(prefix_pair_stability(
                    prev.as_ref()
                        .map(|p| (p.prompt_prefix_hash, p.prompt_tokens)),
                    hash,
                    tokens,
                ))
            }
            _ => None,
        };
        Ok(self
            .manager
            .store()
            .record_provider_call_with_prefix_segments(
                self.id,
                op,
                provider,
                model,
                status,
                tokens_in,
                tokens_out,
                error,
                prompt_prefix_hash,
                prompt_tokens,
                stability,
                prefix_segments_json,
            )
            .map_err(crate::map_store_err)?)
    }

    /// The session's NEWEST durable prefix observation (v13): the last
    /// `provider_call` row of this session carrying a prefix hash, `None`
    /// when none was recorded yet (or the session predates v13). Read-time
    /// store validation is loud: a corrupt previous observation fails the
    /// settlement instead of silently mispairing the stability.
    fn last_prefix_observation(
        &self,
    ) -> faktor_core::Result<Option<faktor_store::ProviderCallPrefixRow>> {
        Ok(self
            .manager
            .store()
            .provider_call_prefix_rows(self.id)
            .map_err(crate::map_store_err)?
            .into_iter()
            .next_back())
    }

    /// The session's stored prefix-stability aggregate (v13): mean/std-dev
    /// over the recorded per-row prefix stabilities, `None` while no
    /// observation row carries one (fresh session or pre-v13 rows only).
    /// Read-only; hostile stored values surface as loud `Malformed`.
    pub fn stored_prefix_stability(
        &self,
    ) -> faktor_core::Result<Option<faktor_store::PrefixStabilityAggregate>> {
        self.manager
            .store()
            .session_stored_prefix_stability(self.id)
            .map_err(crate::map_store_err)
            .map_err(Into::into)
    }

    /// Request permission to use `capability` for `op`. Journals
    /// `ToolRequested` (recorded with state `WaitingForPermission` — the
    /// documented two-hop) and inserts the durable pending row.
    pub fn request_permission(
        &self,
        op: OpId,
        capability: &Capability,
    ) -> faktor_core::Result<PermissionRequest> {
        let _guard = self.command_guard();
        if self.ops().tracked(op).is_none() {
            return Err(SessionError::NotFound(format!("operation {op} is not tracked")).into());
        }
        let cap_json = serde_json::to_string(capability)
            .map_err(|e| SessionError::Malformed(format!("capability serialization: {e}")))?;
        if cap_json.len() > 4096 {
            return Err(SessionError::Oversized("capability too large".into()).into());
        }
        // Validate the ToolRequested hop against the pre-state read ONCE; the
        // atomic store command re-verifies it inside the transaction, so an
        // illegal hop can never leave an orphan pending permission row. The
        // event payload's `permission_id` is stamped by the store from the id
        // it actually allocated (same transaction), never guessed here.
        let current = self.state()?;
        crate::journal::validate_transition(
            current,
            EventKind::ToolRequested,
            AgentState::WaitingForPermission,
        )?;
        let event = command_event(
            EventKind::ToolRequested,
            AgentState::WaitingForPermission,
            Some(op),
            self.now_ms(),
            Some(serde_json::json!({
                "capability": capability_tag(capability),
                "detail": capability,
            })),
        )?;
        let (id, expires_ms, event_seq) = self
            .manager
            .store()
            .insert_permission_and_event(self.id, op, &cap_json, current, event)
            .map_err(crate::map_store_err)?;
        Ok(PermissionRequest {
            id,
            op_id: op,
            capability: capability.clone(),
            event_seq,
            expires_ms,
        })
    }

    /// Resolve a pending permission. `Allow` journals `PermissionGranted`
    /// (state `ExecutingTool`); `Deny` journals `PermissionDenied` and lands
    /// on `ReadyForNextTurn` only when no sibling call of the batch is still
    /// pending — a mixed batch keeps the machine on `ExecutingTool` until
    /// every call resolved, so the approved siblings stay reachable. A double
    /// resolve loses the race with `Conflict`; the journal never records two
    /// resolutions. Resolution is CONTEXTUAL: the durable row must belong to
    /// this session and, while the session tracks live operations, to one of
    /// them — a wrong-window/wrong-session/stale reply is refused typed.
    pub fn resolve_permission(
        &self,
        id: i64,
        decision: faktor_core::capability::PermissionDecision,
    ) -> faktor_core::Result<faktor_core::id::EventSeq> {
        let (decision_str, kind, target) = match decision {
            faktor_core::capability::PermissionDecision::Allow => (
                "allow",
                faktor_core::event::EventKind::PermissionGranted,
                AgentState::ExecutingTool,
            ),
            faktor_core::capability::PermissionDecision::Deny => (
                "deny",
                faktor_core::event::EventKind::PermissionDenied,
                AgentState::ReadyForNextTurn,
            ),
            faktor_core::capability::PermissionDecision::Ask => {
                return Err(SessionError::Malformed(
                    "a permission cannot be resolved with Ask".into(),
                )
                .into());
            }
        };
        let _guard = self.command_guard();
        // The live pending row carries the owning session + op. Unknown,
        // terminal and EXPIRED rows read as None (the durable deadline is the
        // filter); the atomic update below is still invoked in that case so an
        // expired-but-pending row is terminalized and the resolution refused.
        let pending = self
            .manager
            .store()
            .pending_permission(id)
            .map_err(crate::map_store_err)?;
        if let Some((row_session, op, _)) = &pending {
            // Contextual binding: only the session that owns the permission
            // may resolve it (a wrong-window UI reply is refused typed).
            if *row_session != self.id {
                return Err(SessionError::Conflict(format!(
                    "permission {id} is not pending for session {} (owned by {row_session})",
                    self.id
                ))
                .into());
            }
            // Ownership binding: while the session tracks live operations,
            // the permission's owning op must be one of them — a stale reply
            // for an operation the session no longer owns is refused. A
            // restarted daemon tracks nothing (in-process tokens only), so
            // the durable recovery path resolves untracked ops honestly.
            if !self.ops().all().is_empty() && self.ops().tracked(*op).is_none() {
                return Err(SessionError::Conflict(format!(
                    "permission {id} belongs to operation {op}, which is not currently tracked"
                ))
                .into());
            }
        }
        // Deny sibling-awareness (mixed permission batch): the denied call's
        // own result is written only after the whole batch resolves, so at
        // this point the batch may still have unresolved sibling calls (or a
        // sibling already executing). Landing on `ReadyForNextTurn` would
        // claim the batch is finished and make the sibling's next hop
        // (`ToolRequested`, `ToolStarted`, `FileChanged`) illegal. The
        // honest landing while the batch continues is `ExecutingTool`
        // (`WaitingForPermission -> ExecutingTool` is the legal edge); the
        // deny-only batch keeps the documented `ReadyForNextTurn` landing.
        // The pre-state is read ONCE here and re-verified by the atomic store
        // command inside its transaction: the landing decision, the durable
        // row change and the journal event can never disagree.
        let current = self.state()?;
        let target = if kind == EventKind::PermissionDenied {
            self.negative_permission_landing(current)?
        } else {
            target
        };
        crate::journal::validate_transition(current, kind, target)?;
        let event = command_event(
            kind,
            target,
            pending.as_ref().map(|(_, op, _)| *op),
            self.now_ms(),
            Some(serde_json::json!({ "permission_id": id, "decision": decision_str })),
        )?;
        // The atomic update is the arbiter: exactly one row (this session,
        // still pending, still unexpired) must transition; zero is the typed
        // conflict (unknown, already terminal, wrong session, expired). The
        // row change and its journal event commit together (or not at all).
        self.manager
            .store()
            .resolve_permission_and_event(id, self.id, decision_str, current, event)
            .map_err(crate::map_store_err)
            .map_err(Into::into)
    }

    /// Durable sibling evidence for a permission DENIAL: is another tool
    /// call of the session's open batch still unresolved?
    ///
    /// A model tool batch is durable BEFORE any permission hop: every call of
    /// the batch is an assistant `tool_call` part, and the batch's results are
    /// written only after every call resolved. So a `tool_call` part with no
    /// answering `tool_result` (other than the denied call itself) is exactly
    /// "the batch is still open" — regardless of the call's part state (a
    /// `pending`/`running` sibling is just as unresolved as a wire-visible
    /// `completed`/`error` one without a result); a still-running `tool_run`
    /// row is the second, direct signal (a sibling already executing). The
    /// scan is bounded and uses the runtime dangling-call repair's
    /// newest-first cluster walk, deliberately wider than that repair: every
    /// unresolved `tool_call` counts, not only the wire-visible states.
    fn open_batch_has_pending_siblings(&self) -> faktor_core::Result<bool> {
        if !self.pending_tool_runs()?.is_empty() {
            return Ok(true);
        }
        const MAX_SCAN: usize = 128;
        let mut calls: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cursor: Option<i64> = None;
        let mut scanned = 0usize;
        'scan: loop {
            let page = self.messages_before(cursor, 100)?;
            if page.is_empty() {
                break;
            }
            for row in &page {
                if scanned >= MAX_SCAN {
                    break 'scan;
                }
                scanned += 1;
                let mut saw_tool_part = false;
                for part in self.parts_of(row.id)? {
                    let Some(call_id) = part
                        .data
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .filter(|id| !id.is_empty())
                    else {
                        continue;
                    };
                    match part.kind.as_str() {
                        // A batch call part that has no answering result is
                        // unresolved whatever its state: `completed`/`error`
                        // are the wire-visible states, and any OTHER state
                        // (`pending`/`running`/…) is a still-open sibling by
                        // construction. Counting only completed|error made a
                        // pending sibling invisible to the denial landing.
                        "tool_call" => {
                            calls.insert(call_id.to_string());
                            saw_tool_part = true;
                        }
                        "tool_result" => {
                            answered.insert(call_id.to_string());
                            saw_tool_part = true;
                        }
                        _ => {}
                    }
                }
                if !saw_tool_part {
                    break 'scan;
                }
            }
            let Some(oldest) = page.last() else { break };
            if scanned >= MAX_SCAN || oldest.seq <= 1 {
                break;
            }
            cursor = Some(oldest.seq);
        }
        // The denied call is itself unanswered (its result lands after the
        // batch resolves), so a pending sibling exists iff at least two
        // batch calls still await a result.
        Ok(calls.iter().filter(|c| !answered.contains(*c)).count() > 1)
    }

    /// The landing state of a NEGATIVE permission outcome — an explicit
    /// `Deny` or a durable-deadline reconciliation (`PermissionExpired`):
    /// prefer keeping the batch on `ExecutingTool` while any sibling call is
    /// still unresolved (the approved/executing siblings must stay
    /// reachable), else the documented `ReadyForNextTurn`; only ever a LEGAL
    /// machine edge (a self-transition as the last resort), never a forced
    /// hop. Shared verbatim by both paths so an expiry lands exactly where
    /// the equivalent Deny would.
    fn negative_permission_landing(&self, current: AgentState) -> faktor_core::Result<AgentState> {
        let preferred = if self.open_batch_has_pending_siblings()? {
            AgentState::ExecutingTool
        } else {
            AgentState::ReadyForNextTurn
        };
        // Never bypass the machine: pick the first LEGAL landing (a
        // self-transition is legal and idempotent), never a forced one.
        if faktor_core::state::StateMachine::new(current)
            .transition(preferred)
            .is_ok()
        {
            Ok(preferred)
        } else if faktor_core::state::StateMachine::new(current)
            .transition(AgentState::ReadyForNextTurn)
            .is_ok()
        {
            Ok(AgentState::ReadyForNextTurn)
        } else {
            Ok(current)
        }
    }

    /// Reconcile this session's durable permission expiry (P1-E): every
    /// still-`pending` row whose `expires_ms` passed while NO live waiter
    /// existed (the post-restart world) is terminalized as `expired` and
    /// journaled as ONE `PermissionExpired` event — never a fake Deny — in
    /// a single store transaction, landing on the same state an explicit
    /// `Deny` would (sibling-batch aware). Requires the per-session command
    /// guard; called by [`SessionHandle::recover_all_with`] before the
    /// crash state is decided. Idempotent: a second sweep with no expired
    /// rows touches nothing and appends nothing.
    pub fn expire_pending_permissions(
        &self,
    ) -> faktor_core::Result<faktor_store::ExpiredPermissionResolution> {
        let _guard = self.command_guard();
        self.expire_pending_permissions_locked()
    }

    /// [`Self::expire_pending_permissions`] without taking the command guard
    /// (recovery already holds it). Never call this without the guard.
    pub(crate) fn expire_pending_permissions_locked(
        &self,
    ) -> faktor_core::Result<faktor_store::ExpiredPermissionResolution> {
        let now = self.now_ms();
        let expired = self
            .manager
            .store()
            .expired_pending_permissions(self.id, now)
            .map_err(crate::map_store_err)?;
        if expired.is_empty() {
            return Ok(faktor_store::ExpiredPermissionResolution::default());
        }
        let current = self.state()?;
        let target = self.negative_permission_landing(current)?;
        crate::journal::validate_transition(current, EventKind::PermissionExpired, target)?;
        // One sweep, one event: the payload names every terminalized row (the
        // store stamps the ids/ops it actually changed inside the same
        // transaction); a single expired row also carries its op for the
        // journal's op column.
        let op_id = match expired.as_slice() {
            [(_, op)] => Some(*op),
            _ => None,
        };
        let event = command_event(
            EventKind::PermissionExpired,
            target,
            op_id,
            now,
            Some(serde_json::json!({
                "reason": "durable permission deadline elapsed while no live waiter owned the request",
                "expired_count": expired.len(),
            })),
        )?;
        // The atomic store command re-verifies `current` inside its
        // transaction and re-derives the expired set there: the row changes
        // and the journal event commit together or not at all.
        self.manager
            .store()
            .expire_pending_permissions_for_session(self.id, now, current, event)
            .map_err(crate::map_store_err)
            .map_err(Into::into)
    }

    pub fn pending_permission(
        &self,
        id: i64,
    ) -> faktor_core::Result<Option<(faktor_core::id::SessionId, OpId, String)>> {
        self.manager
            .store()
            .pending_permission(id)
            .map_err(|e| crate::map_store_err(e).into())
    }
}

/// Per-row prefix-cache stability of one new observation against the
/// session's previous one (audits 65-66). Deterministic mirror of the
/// documented pair rule in `faktor-router::stability`, which the routing
/// layer applies over the durable rows this twin fills — keep both in
/// lockstep:
///
/// ```text
/// stability = 1.0     first observation (nothing precedes it)
///           = 1.0     either prefix is EMPTY (0 tokens): an empty prefix
///                     destabilizes nothing (documented convention)
///           = 1.0     byte-identical digests: full cache coverage
///           = p / c   strict token growth with different bytes: the only
///                     byte relation consistent with append-only growth —
///                     the previous prefix is fully covered by the current
///           = 0.0     otherwise: same-length or shorter prefix with
///                     different bytes is a REWRITE (reorder/churn)
/// ```
///
/// Always finite in [0, 1]; `cur_tokens` beyond the store's u32 bound is
/// rejected loudly by the insert itself, never silently stored.
fn prefix_pair_stability(
    prev: Option<([u8; 32], u32)>,
    cur_hash: [u8; 32],
    cur_tokens: u64,
) -> f64 {
    let Some((prev_hash, prev_tokens)) = prev else {
        return 1.0;
    };
    if prev_tokens == 0 || cur_tokens == 0 {
        return 1.0;
    }
    if prev_hash == cur_hash {
        return 1.0;
    }
    let (p, c) = (f64::from(prev_tokens), cur_tokens as f64);
    if cur_tokens > u64::from(prev_tokens) {
        p / c
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poisoned_op_registry_is_reconciled_not_propagated() {
        let reg = OpRegistry::default();
        reg.register(OpId::new(1), OpKind::Tool, CancellationToken::new());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = reg.inner.lock().unwrap();
            panic!("holder poisoned the ownership registry");
        }));
        assert!(reg.inner.is_poisoned());
        // Ownership/process => reconcile: the registry keeps serving (the
        // recovered guard, poison cleared) instead of panicking callers;
        // the durable journal remains the authority.
        assert_eq!(reg.all(), vec![OpId::new(1)]);
        assert_eq!(reg.kind(OpId::new(1)), Some(OpKind::Tool));
        reg.unregister(OpId::new(1));
        assert!(reg.all().is_empty());
        assert!(!reg.inner.is_poisoned());
    }

    use crate::handle::tests::{session, test_manager};
    use crate::SessionManager;
    use faktor_core::event::EventKind;
    use faktor_core::id::SessionId;
    use faktor_core::time::Deadline;

    fn op_meta(
        m: &crate::SessionManager,
        s: SessionId,
        recovery: faktor_core::op::RecoveryStrategy,
    ) -> OpMeta {
        let op = m.try_next_op_id().unwrap();
        OpMeta::new(
            op,
            s,
            Deadline::at(m.now_ms() + 60_000),
            faktor_core::retry::RetryPolicy::default(),
            CancellationToken::new(),
            recovery,
            m.now_ms(),
        )
    }

    fn to_streaming(s: &SessionHandle) {
        s.submit_prompt("x", &[]).unwrap();
        s.append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            None,
            None,
        )
        .unwrap();
    }

    fn to_waiting(s: &SessionHandle) {
        to_streaming(s);
        let turn_op = s.ops().all()[0];
        s.request_permission(
            turn_op,
            &Capability::ReadWorkspace {
                path: "/w/a".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn permission_double_resolve_race_single_event() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        let req = s
            .request_permission(
                turn_op,
                &Capability::ExecuteShell {
                    command: "cargo test".into(),
                },
            )
            .unwrap();
        // Two resolvers race: allow vs deny.
        let s = std::sync::Arc::new(s);
        let s1 = s.clone();
        let s2 = s.clone();
        let t1 = std::thread::spawn(move || {
            s1.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
        });
        let t2 = std::thread::spawn(move || {
            s2.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
        });
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert!(
            r1.is_ok() != r2.is_ok(),
            "exactly one resolver must win; got {r1:?} / {r2:?}"
        );
        // Exactly one resolution event, and it matches the winner's decision.
        let events = s.events_range(1, None).unwrap();
        let resolutions: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    EventKind::PermissionGranted | EventKind::PermissionDenied
                )
            })
            .collect();
        assert_eq!(resolutions.len(), 1);
        let winning_decision = if r1.is_ok() { "allow" } else { "deny" };
        let expected_kind = if winning_decision == "allow" {
            EventKind::PermissionGranted
        } else {
            EventKind::PermissionDenied
        };
        assert_eq!(resolutions[0].kind, expected_kind);
    }

    #[test]
    fn start_tool_run_requires_tool_request_hop() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        // From Preparing, ExecutingTool is illegal: the ToolRequested hop is
        // mandatory and the command leaves no trace.
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        let err = s
            .start_tool_run(meta, "read_file", serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(
            err.kind,
            faktor_core::ErrorKind::InvalidState { .. }
        ));
        assert!(s.pending_tool_runs().unwrap().is_empty(), "no tool_run row");
        assert_eq!(s.last_event_seq().unwrap().unwrap().raw(), 2);
    }

    #[test]
    fn foreign_op_meta_rejected_by_start_tool_run() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        s.append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ToolRequested,
            AgentState::WaitingForPermission,
            None,
            None,
        )
        .unwrap();
        // An op envelope pointed at another session is rejected loudly.
        let mut meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        meta.session_id = SessionId::new(999);
        assert!(s
            .start_tool_run(meta, "read", serde_json::json!({}))
            .is_err());
    }

    #[test]
    fn cancelled_tool_run_ends_session_and_unknown_ops_are_loud() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_waiting(&s);
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        let op = meta.operation_id;
        s.start_tool_run(meta, "write_file", serde_json::json!({"path": "a"}))
            .unwrap();
        // Cancelling the run ends the session (terminal by the core machine).
        s.finish_tool_run(op, "cancelled", EffectStatus::Unknown)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::Cancelled);
        assert!(s.pending_tool_runs().unwrap().is_empty());
        // Finishing a finished op is a loud NotFound (no double finish).
        assert!(s
            .finish_tool_run(op, "completed", EffectStatus::Verified)
            .is_err());
        // The registry forgot the op.
        assert!(s.abort(Some(op)).is_err());
    }

    #[test]
    fn completed_tool_run_moves_to_validating_and_unregisters() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_waiting(&s);
        let meta = op_meta(
            &m,
            s.id(),
            faktor_core::op::RecoveryStrategy::VerifyHash {
                path: "/w/a.txt".into(),
                expected: faktor_core::hash::FileHash::from([7; 32]),
            },
        );
        let op = meta.operation_id;
        let handle = s
            .start_tool_run(meta, "write_file", serde_json::json!({"path": "a"}))
            .unwrap();
        assert_eq!(handle.op_id, op);
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
        // The recovery strategy is durable in the row.
        let rows = s.pending_tool_runs().unwrap();
        assert_eq!(rows[0].recovery["strategy"], "verify_hash");
        assert_eq!(
            rows[0].expected_hash.as_deref(),
            Some(faktor_core::hash::FileHash::from([7; 32]).to_hex().as_str())
        );
        s.finish_tool_run(op, "completed", EffectStatus::Verified)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::Validating);
        assert!(s.pending_tool_runs().unwrap().is_empty());
        // Duplicate finish is now a loud NotFound.
        assert!(s
            .finish_tool_run(op, "completed", EffectStatus::Verified)
            .is_err());
    }

    #[test]
    fn request_permission_requires_tracked_op_and_persists() {
        let (_d, m) = test_manager();
        let s = session(&m);
        // No tracked op yet: loud NotFound.
        let err = s
            .request_permission(
                m.try_next_op_id().unwrap(),
                &Capability::Network {
                    destination: "https://x".into(),
                },
            )
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::NotFound);
        // After a prompt the op is tracked; the machine must be at the tool
        // request point before the permission request journals.
        s.submit_prompt("go", &[]).unwrap();
        s.append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
        s.append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            None,
            None,
        )
        .unwrap();
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::Git {
                    operation: "push".into(),
                },
            )
            .unwrap();
        assert_eq!(req.op_id, op);
        assert_eq!(s.state().unwrap(), AgentState::WaitingForPermission);
        // The pending row round-trips to a Capability.
        let (_, rop, cap_str) = s.pending_permission(req.id).unwrap().unwrap();
        assert_eq!(rop, op);
        let cap: Capability = serde_json::from_str(&cap_str).unwrap();
        assert_eq!(
            cap,
            Capability::Git {
                operation: "push".into()
            }
        );
        // Deny returns the session to ready and records PermissionDenied.
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
        // Resolving again conflicts.
        assert!(s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .is_err());
        // The event's payload carries the frozen permission_id + capability.
        let events = s.events_range(1, None).unwrap();
        let req_ev = events
            .iter()
            .find(|e| e.kind == EventKind::ToolRequested)
            .unwrap();
        assert_eq!(req_ev.payload.as_ref().unwrap()["permission_id"], req.id);
        assert_eq!(req_ev.payload.as_ref().unwrap()["capability"], "git");
    }

    #[test]
    fn denied_parallel_tool_stays_executing() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        // Request permission for a tool, then start it before the resolution
        // lands (the agent's auto-approval path): the session is ExecutingTool
        // while the permission is still pending. Denying it now cannot jump
        // ExecutingTool -> ReadyForNextTurn, so it stays executing.
        let req = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/b".into(),
                },
            )
            .unwrap();
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        s.start_tool_run(meta, "read_file", serde_json::json!({"path": "b"}))
            .unwrap();
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
        // The denial was still journaled, with the state the machine actually
        // has.
        let ev = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::PermissionDenied)
            .expect("denial journaled");
        assert_eq!(ev.state, AgentState::ExecutingTool);
        // A second denial while the batch still has a LIVE sibling run stays
        // on the batch-execution edge too: the run is still pending, so the
        // batch is not finished (the old state-only check landed
        // ReadyForNextTurn and stranded the running row's later hops).
        let req2 = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/d".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req2.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
        // Finishing the sibling walks the batch lawfully to its end.
        let rows = s.pending_tool_runs().unwrap();
        assert_eq!(rows.len(), 1);
        s.finish_tool_run(
            rows[0].op_id,
            "completed",
            faktor_core::op::EffectStatus::Verified,
        )
        .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::Validating);
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }

    #[test]
    fn denied_call_with_an_unanswered_sibling_keeps_the_batch_executing() {
        // DEFECT REPRODUCER (mixed permission batch): the model's batch is
        // durable as tool_call parts BEFORE the permission hops, so at deny
        // time the sibling call is durably pending even though its run has
        // not started. The denial must not advance to ReadyForNextTurn — the
        // batch is still open — or the sibling's own permission hop and the
        // later FileChanged/start hop become illegal.
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        let mid = s
            .put_message(
                s.proposed_message_seq().unwrap(),
                "assistant",
                serde_json::json!({ "parts": [] }),
            )
            .unwrap();
        s.put_tool_call_part(mid, "c1", "read_file", serde_json::json!({}), "completed")
            .unwrap();
        s.put_tool_call_part(mid, "c2", "write_file", serde_json::json!({}), "completed")
            .unwrap();
        let req = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/b".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(
            s.state().unwrap(),
            AgentState::ExecutingTool,
            "an unanswered sibling keeps the batch executing"
        );
        // The sibling's own permission hop stays legal and the batch can
        // continue to execute it.
        let req2 = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/c".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req2.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
    }

    #[test]
    fn denied_call_with_a_still_pending_sibling_keeps_the_batch_executing() {
        // DEFECT REPRODUCER (mixed permission batch, non-terminal sibling):
        // the sibling's tool_call part is still `pending` (its permission hop
        // has not even happened), which the old completed|error-only filter
        // ignored — the denial then landed ReadyForNextTurn while the batch
        // was still open and stranding the pending sibling.
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        let mid = s
            .put_message(
                s.proposed_message_seq().unwrap(),
                "assistant",
                serde_json::json!({ "parts": [] }),
            )
            .unwrap();
        s.put_tool_call_part(mid, "c1", "read_file", serde_json::json!({}), "completed")
            .unwrap();
        s.put_tool_call_part(mid, "c2", "write_file", serde_json::json!({}), "pending")
            .unwrap();
        let req = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/p".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(
            s.state().unwrap(),
            AgentState::ExecutingTool,
            "a still-PENDING sibling keeps the batch executing"
        );
        // The pending sibling's own permission hop stays legal.
        let req2 = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/q".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req2.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);

        // CONTROL: a deny-only batch whose single (denied) call is itself
        // still `pending` has NO sibling and keeps the documented landing.
        let s2 = session(&m);
        to_streaming(&s2);
        let turn_op2 = s2.ops().all()[0];
        let mid2 = s2
            .put_message(
                s2.proposed_message_seq().unwrap(),
                "assistant",
                serde_json::json!({ "parts": [] }),
            )
            .unwrap();
        s2.put_tool_call_part(mid2, "only", "read_file", serde_json::json!({}), "pending")
            .unwrap();
        let req3 = s2
            .request_permission(
                turn_op2,
                &Capability::ReadWorkspace {
                    path: "/w/r".into(),
                },
            )
            .unwrap();
        s2.resolve_permission(req3.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(
            s2.state().unwrap(),
            AgentState::ReadyForNextTurn,
            "the denied call alone is not a sibling"
        );
    }

    /// A manager whose clock is manual: durable permission deadlines can be
    /// driven past without sleeping (P1-E).
    fn clocked_manager(
        t0: i64,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<faktor_core::time::TestClock>,
        std::sync::Arc<SessionManager>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(faktor_core::time::TestClock::new(t0));
        let m = SessionManager::open_with_clock(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
            clock.clone(),
        )
        .unwrap();
        (dir, clock, m)
    }

    #[test]
    fn expire_pending_permissions_terminalizes_and_never_fakes_a_deny() {
        let t0 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let (_d, clock, m) = clocked_manager(t0);
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ExecuteShell {
                    command: "cargo test".into(),
                },
            )
            .unwrap();
        // Nothing past the deadline yet: the sweep is invisible (no event,
        // no row change, no state move) even though no live waiter exists.
        let early = s.expire_pending_permissions().unwrap();
        assert!(early.is_empty());
        assert_eq!(early.event_seq, None);
        assert_eq!(s.state().unwrap(), AgentState::WaitingForPermission);
        assert!(s.pending_permission(req.id).unwrap().is_some());
        let seq_before = s.last_event_seq().unwrap().unwrap();
        // Manual clock past the durable deadline: the restart world.
        clock.set(req.expires_ms + 1);
        let resolution = s.expire_pending_permissions().unwrap();
        assert_eq!(resolution.expired, vec![(req.id, op)]);
        assert!(resolution.event_seq.is_some());
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
        assert!(s.pending_permission(req.id).unwrap().is_none());
        assert_eq!(
            m.store().permission_decision(req.id).unwrap().as_deref(),
            Some("expired")
        );
        // The journal explains WHY: PermissionExpired, never a fake Deny.
        let events = s.events_range(1, None).unwrap();
        let expiry: Vec<_> = events
            .iter()
            .filter(|e| e.kind == EventKind::PermissionExpired)
            .collect();
        assert_eq!(expiry.len(), 1, "one sweep, one event");
        assert_eq!(expiry[0].state, AgentState::ReadyForNextTurn);
        assert_eq!(expiry[0].op_id, Some(op));
        assert_eq!(
            expiry[0].payload.as_ref().unwrap()["permission_ids"],
            serde_json::json!([req.id])
        );
        assert!(
            !events.iter().any(|e| matches!(
                e.kind,
                EventKind::PermissionGranted | EventKind::PermissionDenied
            )),
            "expiry is journaled as itself, never laundered through Deny/Granted"
        );
        // A late resolution attempt finds nothing to own: refused, and the
        // durable row stays terminal.
        assert!(s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .is_err());
        assert_eq!(
            m.store().permission_decision(req.id).unwrap().as_deref(),
            Some("expired")
        );
        // Idempotent: a second sweep appends nothing.
        let seq_after = s.last_event_seq().unwrap().unwrap();
        assert!(s.expire_pending_permissions().unwrap().is_empty());
        assert_eq!(s.last_event_seq().unwrap().unwrap(), seq_after);
        assert!(seq_after > seq_before);
    }

    #[test]
    fn expired_permission_mid_batch_lands_by_the_deny_sibling_rule() {
        let t0 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let (_d, clock, m) = clocked_manager(t0);
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        // The model's batch is durable as unanswered tool_call parts before
        // any permission hop: the expired call has a live sibling.
        let mid = s
            .put_message(
                s.proposed_message_seq().unwrap(),
                "assistant",
                serde_json::json!({ "parts": [] }),
            )
            .unwrap();
        s.put_tool_call_part(mid, "c1", "read_file", serde_json::json!({}), "completed")
            .unwrap();
        s.put_tool_call_part(mid, "c2", "write_file", serde_json::json!({}), "completed")
            .unwrap();
        let req = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/p".into(),
                },
            )
            .unwrap();
        clock.set(req.expires_ms + 1);
        let resolution = s.expire_pending_permissions().unwrap();
        assert_eq!(resolution.expired, vec![(req.id, turn_op)]);
        assert_eq!(
            s.state().unwrap(),
            AgentState::ExecutingTool,
            "an unanswered sibling keeps the batch executing — the exact Deny landing"
        );
        let ev = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == EventKind::PermissionExpired)
            .expect("expiry journaled");
        assert_eq!(ev.state, AgentState::ExecutingTool);
        // The still-open sibling's own permission hop stays legal and the
        // batch can continue to execute it.
        let req2 = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/q".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req2.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
    }

    #[test]
    fn genuinely_illegal_transitions_are_still_refused() {
        // CONTROL: the sibling-aware denial path only ever PICKS legal
        // landings — the machine's guards are never weakened. A FileChanged
        // that tries to enter ExecutingTool from ReadyForNextTurn is still
        // refused, and the refusal leaves no trace in the journal or the
        // session row.
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        let req = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
        let before = s.last_event_seq().unwrap().expect("journaled events").raw();
        let err = s
            .append_event(
                EventKind::FileChanged,
                AgentState::ExecutingTool,
                Some(turn_op),
                None,
            )
            .unwrap_err();
        assert!(
            matches!(err.kind, faktor_core::ErrorKind::InvalidState { .. }),
            "a genuinely illegal transition must stay refused: {err:?}"
        );
        assert_eq!(
            s.last_event_seq().unwrap().expect("journaled events").raw(),
            before,
            "no event was appended"
        );
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
        // The tool-request hop is illegal from ReadyForNextTurn too: a
        // permission can never re-open a batch that already ended.
        let err = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/c".into(),
                },
            )
            .unwrap_err();
        assert!(
            matches!(err.kind, faktor_core::ErrorKind::InvalidState { .. }),
            "the ToolRequested hop stays guarded: {err:?}"
        );
        assert!(!s
            .events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == EventKind::ToolRequested && e.seq.raw() > before));
    }

    #[test]
    fn crash_mid_mixed_batch_recovery_resolves_the_row_lawfully() {
        // Crash residue of a MIXED batch: both calls are durable, the
        // approved sibling's run is still running, the denial is journaled
        // and its result was never written. Recovery must resolve the row on
        // a legal path, land the honest recoverable end (never claim the
        // batch finished) and leave a journal that replays cleanly.
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let turn_op = s.ops().all()[0];
        let mid = s
            .put_message(
                s.proposed_message_seq().unwrap(),
                "assistant",
                serde_json::json!({ "parts": [] }),
            )
            .unwrap();
        s.put_tool_call_part(mid, "c_ok", "read_file", serde_json::json!({}), "completed")
            .unwrap();
        s.put_tool_call_part(
            mid,
            "c_deny",
            "write_file",
            serde_json::json!({}),
            "completed",
        )
        .unwrap();
        // The approved sibling is mid-flight at the crash.
        let ok = s
            .request_permission(
                turn_op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        s.resolve_permission(ok.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::Idempotent);
        s.start_tool_run(meta, "read_file", serde_json::json!({}))
            .unwrap();
        // The denial is decided; its durable result part never landed.
        let deny = s
            .request_permission(
                turn_op,
                &Capability::WriteWorkspace {
                    path: "/w/b".into(),
                },
            )
            .unwrap();
        s.resolve_permission(deny.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap();
        assert_eq!(s.state().unwrap(), AgentState::ExecutingTool);
        // CRASH. Recovery resolves the running row honestly.
        let report = s.recover_all().unwrap();
        assert_eq!(report.crashed_ops.len(), 1);
        assert_eq!(report.crashed_ops[0].status, "failed");
        assert_eq!(
            report.crashed_ops[0].effect,
            faktor_core::op::EffectStatus::Unknown
        );
        assert_eq!(report.state, AgentState::FailedRecoverable);
        assert_eq!(s.state().unwrap(), AgentState::FailedRecoverable);
        assert!(s.pending_tool_runs().unwrap().is_empty());
        // The denial and the recovery are durable; the journal replays with
        // no corruption (every landing was legal).
        let events = s.events_range(1, None).unwrap();
        assert!(events.iter().any(|e| e.kind == EventKind::PermissionDenied));
        assert!(events.iter().any(|e| e.kind == EventKind::CrashDetected));
        assert!(events.iter().any(|e| e.kind == EventKind::RecoveryApplied));
        assert_eq!(
            s.replay_journal().unwrap().state,
            AgentState::FailedRecoverable
        );
        // The interrupted turn is over: the session stays promptable and
        // nothing is blindly re-run.
        s.submit_prompt("try again", &[]).unwrap();
        assert_eq!(s.state().unwrap(), AgentState::Preparing);
        assert!(
            s.events_range(1, None)
                .unwrap()
                .iter()
                .filter(|e| e.kind == EventKind::ToolStarted)
                .count()
                == 1
        );
    }

    #[test]
    fn ask_cannot_resolve_permission() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        let err = s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Ask)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Malformed);
        // The permission stays pending and untouched.
        assert!(s.pending_permission(req.id).unwrap().is_some());
    }

    #[test]
    fn resolve_permission_refuses_a_wrong_session() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        // A DIFFERENT session of the same daemon replies (wrong-window UI
        // race): refused typed, and the row is not consumed.
        let other = session(&m);
        let err = other
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
        assert_eq!(
            s.pending_permission(req.id).unwrap().unwrap().0,
            s.id(),
            "the owning session's pending row survives"
        );
        // The owning session still resolves it.
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
    }

    #[test]
    fn resolve_permission_refuses_an_op_the_session_no_longer_tracks() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        // The permission's owning op left the ownership registry while the
        // session still owns another live op: a stale reply is refused.
        s.ops().unregister(op);
        s.ops()
            .register(OpId::new(4242), OpKind::Turn, CancellationToken::new());
        let err = s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
        assert!(
            s.pending_permission(req.id).unwrap().is_some(),
            "the refused resolution leaves the durable row pending"
        );
        // With the session owning nothing (restarted daemon shape: in-process
        // tokens are gone), the durable recovery path resolves honestly.
        s.ops().unregister(OpId::new(4242));
        s.resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap();
    }

    #[test]
    fn resolve_permission_terminalizes_an_expired_row_and_refuses() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        // Force the durable deadline into the past (the sql seam exists for
        // exactly this kind of adversarial craft).
        s.manager()
            .store()
            .sql_execute(&format!(
                "UPDATE permission SET expires_ms = 0 WHERE id = {}",
                req.id
            ))
            .unwrap();
        assert!(s.pending_permission(req.id).unwrap().is_none());
        let err = s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Allow)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
        assert_eq!(
            s.manager()
                .store()
                .permission_decision(req.id)
                .unwrap()
                .as_deref(),
            Some("expired"),
            "the refused attempt terminalized the row"
        );
        // Terminal stays terminal.
        let err = s
            .resolve_permission(req.id, faktor_core::capability::PermissionDecision::Deny)
            .unwrap_err();
        assert_eq!(err.kind, faktor_core::ErrorKind::Conflict, "{err}");
    }

    #[test]
    fn oversized_tool_args_rejected_before_write() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        for (k, st) in [
            (EventKind::ContextPrepared, AgentState::BuildingContext),
            (EventKind::ModelStarted, AgentState::WaitingForModel),
            (EventKind::ModelChunkReceived, AgentState::Streaming),
            (EventKind::ToolRequested, AgentState::WaitingForPermission),
        ] {
            s.append_event(k, st, None, None).unwrap();
        }
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        let big_args = serde_json::json!({ "blob": "x".repeat(MAX_TOOL_ARGS_BYTES + 1) });
        assert!(s.start_tool_run(meta, "run", big_args).is_err());
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }

    #[test]
    fn expired_or_cancelled_op_meta_is_rejected_before_start() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        for (k, st) in [
            (EventKind::ContextPrepared, AgentState::BuildingContext),
            (EventKind::ModelStarted, AgentState::WaitingForModel),
            (EventKind::ModelChunkReceived, AgentState::Streaming),
            (EventKind::ToolRequested, AgentState::WaitingForPermission),
        ] {
            s.append_event(k, st, None, None).unwrap();
        }
        // Deadline already in the past.
        let mut meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        meta.deadline = Deadline::at(m.now_ms() - 1);
        assert!(s
            .start_tool_run(meta, "read", serde_json::json!({}))
            .is_err());
        // Cancelled token.
        let token = CancellationToken::new();
        token.cancel();
        let mut meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        meta.cancellation = token;
        assert!(s
            .start_tool_run(meta, "read", serde_json::json!({}))
            .is_err());
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }

    #[test]
    fn provider_call_records_are_durable_and_bounded() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let op = m.try_next_op_id().unwrap();
        let id = s
            .record_provider_call(op, "ollama", "qwen3.8", "ok", Some(100), Some(50), None)
            .unwrap();
        assert!(id > 0);
        let huge = "p".repeat(300);
        assert!(s
            .record_provider_call(op, &huge, "m", "ok", None, None, None)
            .is_err());
    }

    #[test]
    fn abort_cancels_tool_token_before_durable_updates() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_waiting(&s);
        let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
        let op = meta.operation_id;
        s.start_tool_run(meta, "run", serde_json::json!({}))
            .unwrap();
        let tracked = s.ops().tracked(op).unwrap();
        assert!(!tracked.token.is_cancelled());
        let receipt = s.abort(Some(op)).unwrap();
        assert_eq!(receipt.op_ids, vec![op]);
        assert!(
            tracked.token.is_cancelled(),
            "abort must cancel the op token"
        );
        // The durable row is finished with cancelled/unknown.
        let rows = s.pending_tool_runs().unwrap();
        assert!(rows.is_empty());
        // Aborting the tool op journals exactly one ToolCancelled event; the
        // turn op is untouched (a tool abort does not kill the session turn).
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == EventKind::ToolCancelled)
                .count(),
            1
        );
        assert!(!kinds.contains(&EventKind::Failed));
        // A tool abort cancels the tool, not the session: the machine lands
        // ready for the next prompt (review P0-2).
        assert_eq!(s.state().unwrap(), AgentState::ReadyForNextTurn);
    }

    #[test]
    fn tool_run_carries_replay_descriptor_attempt_and_postcondition() {
        // v7: an idempotent run's row stores its replay descriptor; the
        // attempt counter starts at 0 and a recovery replay bumps it; the
        // workspace-write postcondition is annotated before the finish.
        let (_d, m) = test_manager();
        let s = session(&m);
        to_waiting(&s);
        let mut meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::Idempotent);
        meta = meta.with_replay(serde_json::json!({
            "tool_name": "echo",
            "validated_args": {"x": 1},
        }));
        let op = meta.operation_id;
        let handle = s
            .start_tool_run(meta, "echo", serde_json::json!({"x": 1}))
            .unwrap();
        let rows = s.pending_tool_runs().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempt, 0, "original physical attempt is 0");
        assert_eq!(
            rows[0].replay_descriptor.as_ref().unwrap()["tool_name"],
            "echo"
        );
        assert_eq!(s.bump_tool_attempt(op).unwrap(), 1, "a replay is attempt 1");
        let pc = serde_json::json!({
            "workspace_id": 1,
            "worktree_id": 1,
            "relative_path": "a.txt",
            "expected_hash": "ab".repeat(32),
        });
        s.record_tool_postcondition(op, &pc).unwrap();
        assert_eq!(
            s.pending_tool_runs().unwrap()[0]
                .postcondition
                .as_ref()
                .unwrap()["relative_path"],
            "a.txt"
        );
        // Annotation of a finished row is loud (never a silent ignore).
        s.finish_tool_run(op, "completed", EffectStatus::Applied)
            .unwrap();
        assert!(s.record_tool_postcondition(op, &pc).is_err());
        assert!(s.bump_tool_attempt(op).is_err());
        assert_eq!(handle.op_id, op);
        // The row kept ONE identity throughout: same op, no duplicates.
        assert!(s.pending_tool_runs().unwrap().is_empty());
    }

    // ------------------------------------------------- prefix-cache rows
    // (audits 65-66 fill site): settle_usage_with_prefix records
    // byte-truth prefix observations and derives the per-row stability
    // against the session's previous observation.

    /// Test-only digest: distinct byte strings map to distinct 32-byte
    /// digests (FNV-1a lanes, the same shape the router stability tests
    /// use — no external hash dependency needed for row math).
    fn test_digest(bytes: &[u8]) -> [u8; 32] {
        let mut out = [0u8; 32];
        for (k, basis) in [
            0xcbf29ce484222325u64,
            0x84222325,
            0x9e3779b97f4a7c15,
            0x100000001b3,
        ]
        .iter()
        .enumerate()
        {
            let mut h = *basis ^ 0xdead_beef_1234_5678u64.wrapping_mul(k as u64 + 1);
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x100000001b3);
            }
            let lane = h.to_le_bytes();
            out[k * 8..k * 8 + 8].copy_from_slice(&lane);
        }
        out
    }

    fn prefix_rows(s: &SessionHandle) -> Vec<faktor_store::ProviderCallPrefixRow> {
        s.manager.store().provider_call_prefix_rows(s.id()).unwrap()
    }

    fn settle_bytes(s: &SessionHandle, op: OpId, bytes: &[u8]) -> i64 {
        // Byte-truth settle: hash the EXACT prefix bytes and count the
        // bytes as the token proxy — equal bytes → equal count, so
        // consecutive identical states are byte-identical observations.
        s.settle_usage_with_prefix(
            op,
            "fake",
            "m",
            "completed",
            Some(100),
            Some(50),
            None,
            Some(test_digest(bytes)),
            Some(bytes.len().max(1) as u64),
        )
        .unwrap()
    }

    #[test]
    fn prefix_observations_record_stability_over_turns_and_survive_reopen() {
        // The fill contract end to end at the session chokepoint: after
        // three byte-identical turns the rows carry hash/tokens/stability
        // (1.0 — byte truth), a reordering turn (same token count, rewritten
        // bytes) records 0.0, an append-consistent growth records the
        // coverage ratio, and every observation survives a store reopen.
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let s = session(&m);
        let sid = s.id();
        let op = || m.try_next_op_id().unwrap();
        // Turn 1-3: identical prompt heads.
        settle_bytes(&s, op(), b"static prefix bytes");
        settle_bytes(&s, op(), b"static prefix bytes");
        settle_bytes(&s, op(), b"static prefix bytes");
        // Turn 4: a REWRITE of the head — same estimated token count (the
        // estimator counts chars/3 vs chars/3.4; these differ by one byte,
        // same token estimate... choose equal-length distinct bytes).
        settle_bytes(&s, op(), b"static prefix bxtes");
        // Turn 5: append-consistent growth (old head is a byte-prefix).
        settle_bytes(&s, op(), b"static prefix bytes plus more");

        let rows = prefix_rows(&s);
        assert_eq!(rows.len(), 5, "one observation per settled call");
        assert!(
            rows.iter()
                .all(|r| r.prompt_tokens > 0 && r.prefix_stability.is_some()),
            "every observation row must carry tokens and stability: {rows:?}"
        );
        assert_eq!(
            rows[0].prefix_stability,
            Some(1.0),
            "first observation is stable by definition"
        );
        assert_eq!(rows[1].prefix_stability, Some(1.0), "byte-identical head");
        assert_eq!(rows[2].prefix_stability, Some(1.0), "byte-identical head");
        assert_eq!(
            rows[3].prefix_stability,
            Some(0.0),
            "same-count rewrite is 0.0"
        );
        // Row 5: growth with a different digest is append-consistent.
        let p = rows[3].prompt_tokens as f64;
        let c = rows[4].prompt_tokens as f64;
        assert_eq!(rows[4].prefix_stability, Some(p / c));
        // Hashes are byte truth: equal bytes → equal digests.
        assert_eq!(rows[0].prompt_prefix_hash, rows[1].prompt_prefix_hash);
        assert_eq!(rows[0].prompt_prefix_hash, rows[2].prompt_prefix_hash);
        assert_ne!(rows[3].prompt_prefix_hash, rows[4].prompt_prefix_hash);
        // Store aggregate mirrors the recorded series (mean incl. row 1).
        let agg = s.stored_prefix_stability().unwrap().unwrap();
        assert_eq!(agg.observations, 5);
        let mean = (1.0 + 1.0 + 1.0 + 0.0 + p / c) / 5.0;
        assert!((agg.mean - mean).abs() < 1e-12);

        // Reopen: the rows are durable — release every handle, then a fresh
        // manager over the same dir reads the identical observations and
        // aggregate.
        drop(s);
        drop(m);
        let m2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        let rows2 = prefix_rows(&s2);
        assert_eq!(rows2, rows, "observations must survive the reopen");
        let agg2 = s2.stored_prefix_stability().unwrap().unwrap();
        assert_eq!(agg2.observations, agg.observations);
        assert!((agg2.mean - agg.mean).abs() < 1e-12);
        // A new observation on the reopened session chains off the last row.
        settle_bytes(&s2, m2.try_next_op_id().unwrap(), b"static prefix bytes");
        let rows3 = prefix_rows(&s2);
        assert_eq!(rows3.len(), 6);
        assert_eq!(
            rows3[5].prefix_stability,
            Some(0.0),
            "row 6 vs the grown row 5 is a shrink-rewrite"
        );
    }

    /// A strict v19 segment observation payload: distinct 64-hex digests
    /// (one per token count), deterministic cache reads.
    fn segments_json(seed: u8, tokens: &[u64]) -> String {
        let hashes: Vec<String> = (0..tokens.len())
            .map(|i| format!("{:02x}", seed.wrapping_add(i as u8)).repeat(32))
            .collect();
        serde_json::json!({
            "segment_hashes": hashes,
            "segment_token_counts": tokens,
            "cache_read_tokens": 11u64,
        })
        .to_string()
    }

    #[test]
    fn prefix_segment_payloads_round_trip_across_reopen_and_corruption_is_loud() {
        // The additive v19 settlement payload lands verbatim on the row,
        // chains per session, survives a reopen, and a malformed payload is
        // refused loudly at the write gate (the read gate is covered by the
        // store's own corruption test).
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let s = session(&m);
        let sid = s.id();
        let op = || m.try_next_op_id().unwrap();
        let seg1 = segments_json(0x10, &[10, 20, 30, 40, 50, 5, 4, 3]);
        let seg2 = segments_json(0x10, &[10, 20, 30, 40, 50, 5, 9, 2]);
        s.settle_usage_with_prefix_segments(
            op(),
            "fake",
            "m",
            "completed",
            Some(9),
            Some(1),
            None,
            Some(test_digest(b"head one")),
            Some(150),
            Some(&seg1),
        )
        .unwrap();
        s.settle_usage_with_prefix_segments(
            op(),
            "fake",
            "m",
            "completed",
            Some(9),
            Some(1),
            None,
            Some(test_digest(b"head two")),
            Some(150),
            // The second call changed only volatile segments — the router
            // reads this back to compute the stable leading tokens.
            Some(&seg2),
        )
        .unwrap();
        // The legacy twin records the same row with a NULL payload.
        settle_bytes(&s, op(), b"legacy head");
        let rows = prefix_rows(&s);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].prefix_segments_json.as_deref(), Some(seg1.as_str()));
        assert_eq!(rows[1].prefix_segments_json.as_deref(), Some(seg2.as_str()));
        assert_eq!(rows[2].prefix_segments_json, None);
        assert!(
            rows.iter().all(|r| r.prefix_stability.is_some()),
            "segment payloads must never disable the stability chain: {rows:?}"
        );
        // Hostile writes are typed refusals before the row exists.
        let before = prefix_rows(&s).len();
        let err = s
            .settle_usage_with_prefix_segments(
                op(),
                "fake",
                "m",
                "completed",
                None,
                None,
                None,
                Some(test_digest(b"hostile")),
                Some(1),
                Some("{"),
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("prefix_segments_json"),
            "the refusal must name the corrupt payload: {err}"
        );
        assert_eq!(prefix_rows(&s).len(), before, "nothing may land");
        // Reopen: byte-identical rows through a fresh manager.
        drop(s);
        drop(m);
        let m2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let s2 = m2.get_session(sid).unwrap().unwrap();
        assert_eq!(prefix_rows(&s2), rows, "payloads must survive the reopen");
    }

    #[test]
    fn prefix_observations_are_per_session_and_prefix_less_rows_never_chain() {
        // Two sessions interleave: each session's stability chains against
        // ITS OWN previous observation only, and rows settled without a
        // prefix (started/failed frames, pre-v13 shapes) never join the
        // observation series nor move the chain.
        let (_d, m) = test_manager();
        let a = session(&m);
        let b = session(&m);
        let op = || m.try_next_op_id().unwrap();
        settle_bytes(&a, op(), b"session A head");
        settle_bytes(&b, op(), b"session B head");
        settle_bytes(&a, op(), b"session A head");
        // Prefix-less frame between A's observations: a NULL-prefix row that
        // must not appear in the series nor reset the chain.
        a.record_provider_call(op(), "fake", "m", "started", None, None, None)
            .unwrap();
        settle_bytes(&a, op(), b"session A head");
        let ra = prefix_rows(&a);
        assert_eq!(ra.len(), 3);
        assert!(
            ra.iter().all(|r| r.prefix_stability == Some(1.0)),
            "A's chain must stay stable: {ra:?}"
        );
        let rb = prefix_rows(&b);
        assert_eq!(rb.len(), 1);
        assert_eq!(rb[0].prefix_stability, Some(1.0));
        // A's rows never leak into B's chain: rewrite A, B stays 1.0.
        settle_bytes(&a, op(), b"session A heab");
        let rb = prefix_rows(&b);
        assert_eq!(rb.len(), 1);
        assert_eq!(rb[0].prefix_stability, Some(1.0));
        let ra = prefix_rows(&a);
        assert_eq!(ra.len(), 4);
        assert_eq!(ra[3].prefix_stability, Some(0.0));
    }

    #[test]
    fn prefix_pair_stability_rule_is_deterministic_and_total() {
        // Adversarial row math: every branch of the mirror rule, including
        // hostile inputs that must never panic or escape [0, 1].
        let h = test_digest(b"head");
        assert_eq!(prefix_pair_stability(None, h, 10), 1.0);
        // Empty prefixes (0 tokens) destabilize nothing.
        assert_eq!(prefix_pair_stability(Some((h, 0)), h, 10), 1.0);
        assert_eq!(prefix_pair_stability(Some((h, 10)), h, 0), 1.0);
        // Identical digests win over any count difference.
        assert_eq!(prefix_pair_stability(Some((h, 10)), h, 5), 1.0);
        assert_eq!(prefix_pair_stability(Some((h, 10)), h, 10), 1.0);
        assert_eq!(prefix_pair_stability(Some((h, 10)), h, u64::MAX), 1.0);
        // Different digest, strict growth: coverage ratio.
        let g = test_digest(b"head plus");
        assert_eq!(prefix_pair_stability(Some((h, 10)), g, 20), 0.5);
        assert_eq!(prefix_pair_stability(Some((h, 3)), g, 4), 0.75);
        assert_eq!(prefix_pair_stability(Some((h, 1)), g, 2), 0.5);
        // Different digest, equal or shorter: rewrite → 0.0.
        assert_eq!(prefix_pair_stability(Some((h, 10)), g, 10), 0.0);
        assert_eq!(prefix_pair_stability(Some((h, 10)), g, 9), 0.0);
        // A hostile u64 current count with a u32-max previous count is
        // strict growth: the append-consistent coverage ratio, never 0/NaN.
        assert_eq!(
            prefix_pair_stability(Some((h, u32::MAX)), g, u64::MAX),
            f64::from(u32::MAX) / u64::MAX as f64
        );
        // Always finite and in [0, 1].
        for (p, cur) in [
            (Some((h, 1)), 3u64),
            (Some((h, u32::MAX)), 3u64),
            (None, 0u64),
        ] {
            let v = prefix_pair_stability(p, g, cur);
            assert!(v.is_finite() && (0.0..=1.0).contains(&v));
        }
    }

    #[test]
    fn settle_usage_with_prefix_rejects_hostile_inputs_loudly() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let h = test_digest(b"head");
        // Oversized provider/model names are rejected before any write.
        let huge = "p".repeat(300);
        assert!(s
            .settle_usage_with_prefix(
                m.try_next_op_id().unwrap(),
                &huge,
                "m",
                "completed",
                None,
                None,
                None,
                Some(h),
                Some(1),
            )
            .is_err());
        assert!(s
            .settle_usage_with_prefix(
                m.try_next_op_id().unwrap(),
                "fake",
                &"m".repeat(300),
                "completed",
                None,
                None,
                None,
                Some(h),
                Some(1),
            )
            .is_err());
        // A prompt token count beyond the u32 bound fails loudly (the
        // store's oversized guard surfaces through the session twin's
        // error mapping) and writes nothing.
        let err = s
            .settle_usage_with_prefix(
                m.try_next_op_id().unwrap(),
                "fake",
                "m",
                "completed",
                None,
                None,
                None,
                Some(h),
                Some(1 << 33),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("u32 prefix-token bound"),
            "the store's loud oversized guard must surface: {err}"
        );
        // Nothing landed (the first hostile settle was also rejected).
        assert!(prefix_rows(&s).is_empty());
        // Prefix-less settles land with NULL prefix columns and are excluded
        // from the observation series (like plain settle_usage rows).
        s.record_provider_call(
            m.try_next_op_id().unwrap(),
            "fake",
            "m",
            "started",
            None,
            None,
            None,
        )
        .unwrap();
        assert!(prefix_rows(&s).is_empty());
        assert!(s.stored_prefix_stability().unwrap().is_none());
    }

    #[tokio::test]
    async fn record_provider_call_attempt_requires_a_fresh_attempt_op_id_and_returns_its_own_row() {
        // Attempt-oriented writer: a physical attempt must carry an attempt
        // op id distinct from its logical parent (the audit hole: rows
        // shared one op id across attempts), and each write is its own row.
        let (_d, m) = test_manager();
        let s = session(&m);
        let logical = m.try_next_op_id().unwrap();
        let a0 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 0).unwrap();
        let a1 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 1).unwrap();

        // Distinct attempts -> distinct durable rows, distinct row ids.
        let p0 = s
            .record_provider_call_attempt(a0, None, "fake", "m", "completed", Some(10), None, None)
            .unwrap();
        let p1 = s
            .record_provider_call_attempt(
                a1,
                None,
                "fake",
                "m",
                "completed",
                Some(30),
                Some(40),
                None,
            )
            .unwrap();
        assert_ne!(p0, p1);

        // An attempt that reuses its logical op id is rejected loudly
        // (a hostile deserialized attempt, never silently a second row of
        // the same op).
        let forged = ModelCallAttempt {
            logical_op_id: logical,
            attempt_op_id: logical,
            ordinal: 2,
        };
        let err = s
            .record_provider_call_attempt(forged, None, "fake", "m", "started", None, None, None)
            .unwrap_err();
        assert!(
            err.to_string().contains("must differ"),
            "a reused op id is the exact audit hole: {err}"
        );
        // A fabricated reservation reference (id 0) is rejected loudly
        // rather than persisted as a link to nothing.
        let err = s
            .record_provider_call_attempt(
                ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 2).unwrap(),
                Some(crate::budget::ReservationId::new(0)),
                "fake",
                "m",
                "started",
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("not a durable reservation"),
            "a zero reservation id must never persist: {err}"
        );
        // Oversized provider/model stays typed before any write.
        let big = "x".repeat(257);
        assert!(s
            .record_provider_call_attempt(a0, None, &big, "m", "started", None, None, None)
            .unwrap_err()
            .to_string()
            .contains("too long"));
    }

    // ------------------------------------------------ atomic command seams
    //
    // One logical session command = one SQLite transaction, so a crash at any
    // of its durability boundaries must reopen on EXACTLY the old world or
    // EXACTLY the new one — never a side row without its journal transition.

    const COMMAND_SEAMS: [&str; 3] = [
        "session_command_side_row",
        "session_command_precommit",
        "session_command_committed",
    ];

    fn reopen(dir: &tempfile::TempDir) -> std::sync::Arc<crate::SessionManager> {
        crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap()
    }

    /// Arm `seam`, run the command (it must panic at the boundary), drop the
    /// crashed daemon and reopen from disk; the handle for the same session
    /// comes back for the old/new assertions.
    fn crash_and_reopen(
        dir: &tempfile::TempDir,
        m: std::sync::Arc<crate::SessionManager>,
        s: SessionHandle,
        seam: &'static str,
        run: impl FnOnce(&SessionHandle),
    ) -> SessionHandle {
        let sid = s.id();
        m.store().crash_arm(faktor_store::CrashArm {
            point: seam,
            ordinal: 0,
        });
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&s)));
        assert!(caught.is_err(), "seam {seam} must fire");
        drop(s);
        drop(m);
        reopen(dir).get_session(sid).unwrap().unwrap()
    }

    fn event_kinds(s: &SessionHandle) -> Vec<EventKind> {
        s.events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect()
    }

    #[test]
    fn request_permission_seams_reopen_old_or_new_only() {
        for seam in COMMAND_SEAMS {
            let (dir, m) = test_manager();
            let s = session(&m);
            to_streaming(&s);
            let op = s.ops().all()[0];
            let after = crash_and_reopen(&dir, m, s, seam, |s| {
                let _ = s.request_permission(
                    op,
                    &Capability::ReadWorkspace {
                        path: "/w/a".into(),
                    },
                );
            });
            let requested = event_kinds(&after)
                .iter()
                .filter(|k| **k == EventKind::ToolRequested)
                .count();
            match seam {
                "session_command_committed" => {
                    assert_eq!(requested, 1, "committed request journals once");
                    assert_eq!(after.state().unwrap(), AgentState::WaitingForPermission);
                    // The journaled id names the durable pending row.
                    let ev = after
                        .events_range(1, None)
                        .unwrap()
                        .into_iter()
                        .find(|e| e.kind == EventKind::ToolRequested)
                        .unwrap();
                    let pid = ev.payload.as_ref().unwrap()["permission_id"]
                        .as_i64()
                        .expect("permission_id");
                    assert!(after.pending_permission(pid).unwrap().is_some());
                }
                _ => {
                    assert_eq!(requested, 0, "rolled-back request leaves no event");
                    assert_eq!(after.state().unwrap(), AgentState::Streaming);
                }
            }
        }
    }

    #[test]
    fn resolve_permission_seams_reopen_old_or_new_only() {
        for seam in COMMAND_SEAMS {
            let (dir, m) = test_manager();
            let s = session(&m);
            to_streaming(&s);
            let op = s.ops().all()[0];
            let req = s
                .request_permission(
                    op,
                    &Capability::ReadWorkspace {
                        path: "/w/a".into(),
                    },
                )
                .unwrap();
            let id = req.id;
            let after = crash_and_reopen(&dir, m, s, seam, |s| {
                let _ =
                    s.resolve_permission(id, faktor_core::capability::PermissionDecision::Allow);
            });
            let granted = event_kinds(&after)
                .iter()
                .filter(|k| **k == EventKind::PermissionGranted)
                .count();
            match seam {
                "session_command_committed" => {
                    assert_eq!(granted, 1, "committed resolution journals once");
                    assert_eq!(after.state().unwrap(), AgentState::ExecutingTool);
                    assert_eq!(
                        after
                            .manager()
                            .store()
                            .permission_decision(id)
                            .unwrap()
                            .as_deref(),
                        Some("allow")
                    );
                }
                _ => {
                    assert_eq!(granted, 0, "rolled-back resolution leaves no event");
                    assert_eq!(after.state().unwrap(), AgentState::WaitingForPermission);
                    assert!(
                        after.pending_permission(id).unwrap().is_some(),
                        "the permission stays pending"
                    );
                }
            }
        }
    }

    #[test]
    fn start_tool_run_seams_reopen_old_or_new_only() {
        for seam in COMMAND_SEAMS {
            let (dir, m) = test_manager();
            let s = session(&m);
            to_waiting(&s);
            let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
            let op = meta.operation_id;
            let after = crash_and_reopen(&dir, m, s, seam, |s| {
                let _ = s.start_tool_run(meta, "read_file", serde_json::json!({}));
            });
            let started = event_kinds(&after)
                .iter()
                .filter(|k| **k == EventKind::ToolStarted)
                .count();
            let running = after.pending_tool_runs().unwrap();
            match seam {
                "session_command_committed" => {
                    assert_eq!(started, 1, "committed start journals once");
                    assert_eq!(running.len(), 1);
                    assert_eq!(running[0].op_id, op);
                    assert_eq!(after.state().unwrap(), AgentState::ExecutingTool);
                }
                _ => {
                    assert_eq!(started, 0, "rolled-back start leaves no event");
                    assert!(running.is_empty(), "no tool_run row without its event");
                    assert_eq!(after.state().unwrap(), AgentState::WaitingForPermission);
                }
            }
        }
    }

    #[test]
    fn finish_tool_run_seams_reopen_old_or_new_only() {
        for seam in COMMAND_SEAMS {
            let (dir, m) = test_manager();
            let s = session(&m);
            to_waiting(&s);
            let meta = op_meta(&m, s.id(), faktor_core::op::RecoveryStrategy::None);
            let op = meta.operation_id;
            s.start_tool_run(meta, "read_file", serde_json::json!({}))
                .unwrap();
            let after = crash_and_reopen(&dir, m, s, seam, |s| {
                let _ = s.finish_tool_run(op, "completed", EffectStatus::Verified);
            });
            let completed = event_kinds(&after)
                .iter()
                .filter(|k| **k == EventKind::ToolCompleted)
                .count();
            match seam {
                "session_command_committed" => {
                    assert_eq!(completed, 1, "committed finish journals once");
                    assert!(after.pending_tool_runs().unwrap().is_empty());
                    assert_eq!(after.state().unwrap(), AgentState::Validating);
                }
                _ => {
                    assert_eq!(completed, 0, "rolled-back finish leaves no event");
                    let running = after.pending_tool_runs().unwrap();
                    assert_eq!(running.len(), 1, "the row is still running");
                    assert_eq!(running[0].op_id, op);
                    assert_eq!(after.state().unwrap(), AgentState::ExecutingTool);
                }
            }
        }
    }

    #[test]
    fn barrier_concurrent_duplicate_resolution_has_one_winner() {
        let (_d, m) = test_manager();
        let s = session(&m);
        to_streaming(&s);
        let op = s.ops().all()[0];
        let req = s
            .request_permission(
                op,
                &Capability::ExecuteShell {
                    command: "cargo test".into(),
                },
            )
            .unwrap();
        let s = std::sync::Arc::new(s);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let spawn = |decision: faktor_core::capability::PermissionDecision| {
            let s = s.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                s.resolve_permission(req.id, decision)
            })
        };
        let t1 = spawn(faktor_core::capability::PermissionDecision::Allow);
        let t2 = spawn(faktor_core::capability::PermissionDecision::Deny);
        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();
        assert!(
            r1.is_ok() != r2.is_ok(),
            "exactly one resolver must win; got {r1:?} / {r2:?}"
        );
        let resolutions: Vec<_> = event_kinds(&s)
            .into_iter()
            .filter(|k| {
                matches!(
                    k,
                    EventKind::PermissionGranted | EventKind::PermissionDenied
                )
            })
            .collect();
        assert_eq!(resolutions.len(), 1, "one resolution, one event");
        let expected = if r1.is_ok() {
            EventKind::PermissionGranted
        } else {
            EventKind::PermissionDenied
        };
        assert_eq!(resolutions[0], expected);
        let decision = s.manager().store().permission_decision(req.id).unwrap();
        assert_eq!(
            decision.as_deref(),
            Some(if expected == EventKind::PermissionGranted {
                "allow"
            } else {
                "deny"
            })
        );
    }
}
