//! Compaction records with the hard invariant from section 9 of the
//! architecture spec: a successful compaction must achieve the configured
//! minimum reduction. A "summary" that reduces context by ~1% is rejected.

use faktor_core::event::EventKind;

use crate::handle::SessionHandle;
use crate::SessionError;

/// Compaction acceptance policy.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionPolicy {
    /// Minimum reduction ratio (1 - after/before) a compaction must achieve
    /// to be accepted. Must be in `[0.0, 1.0)`.
    pub min_reduction_ratio: f64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            min_reduction_ratio: 0.25,
        }
    }
}

/// The durable lifecycle state of one explicit compaction operation
/// (S10/K-03). `Cancelling` is set by a concurrent cancel request and is
/// observed by the executor before any commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionOperationState {
    Prepared,
    Planning,
    Reducing,
    Committing,
    Applied,
    Refused,
    Cancelling,
    Cancelled,
    Failed,
    /// The daemon stopped during the commit phase. The checkpoint/event
    /// commit is atomic, but without the executor there is no proof whether
    /// it landed: this state says exactly that instead of guessing
    /// applied/failed. Reconciliation on the next read inspects the durable
    /// checkpoint/compaction rows.
    EffectUnknown,
}

impl CompactionOperationState {
    pub fn as_str(self) -> &'static str {
        match self {
            CompactionOperationState::Prepared => "prepared",
            CompactionOperationState::Planning => "planning",
            CompactionOperationState::Reducing => "reducing",
            CompactionOperationState::Committing => "committing",
            CompactionOperationState::Applied => "applied",
            CompactionOperationState::Refused => "refused",
            CompactionOperationState::Cancelling => "cancelling",
            CompactionOperationState::Cancelled => "cancelled",
            CompactionOperationState::Failed => "failed",
            CompactionOperationState::EffectUnknown => "effect_unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "prepared" => Self::Prepared,
            "planning" => Self::Planning,
            "reducing" => Self::Reducing,
            "committing" => Self::Committing,
            "applied" => Self::Applied,
            "refused" => Self::Refused,
            "cancelling" => Self::Cancelling,
            "cancelled" => Self::Cancelled,
            "failed" => Self::Failed,
            "effect_unknown" => Self::EffectUnknown,
            _ => return None,
        })
    }

    /// Terminal states never transition again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Applied | Self::Refused | Self::Cancelled | Self::Failed | Self::EffectUnknown
        )
    }
}

/// One durable compaction operation (S10/K-03/K-04), projected from the
/// `compaction_operation` row. Capability for the UI: state/phase/progress
/// are durable, not process-local, so a restart resumes at the truthful
/// state instead of inventing progress.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionOperation {
    pub operation_id: String,
    pub state: CompactionOperationState,
    pub phase: String,
    pub progress_ppm: i64,
    pub expected_revision: Option<u64>,
    pub manifest_revision: Option<u64>,
    pub before_tokens: Option<i64>,
    pub after_tokens: Option<i64>,
    pub target_tokens: i64,
    pub min_reduction_ppm: i64,
    pub strategy: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl CompactionOperation {
    fn from_row(row: faktor_store::CompactionOperationRow) -> faktor_core::Result<Self> {
        let state = CompactionOperationState::parse(&row.state).ok_or_else(|| {
            SessionError::Malformed(format!(
                "compaction operation {} has unknown state {}",
                row.operation_id, row.state
            ))
        })?;
        let result = match row.result_json {
            Some(raw) => Some(serde_json::from_str(&raw).map_err(|e| {
                SessionError::Malformed(format!(
                    "compaction operation {} result is malformed: {e}",
                    row.operation_id
                ))
            })?),
            None => None,
        };
        Ok(Self {
            operation_id: row.operation_id,
            state,
            phase: row.phase,
            progress_ppm: row.progress_ppm.clamp(0, 1_000_000),
            expected_revision: row.expected_revision.and_then(|v| u64::try_from(v).ok()),
            manifest_revision: row.manifest_revision.and_then(|v| u64::try_from(v).ok()),
            before_tokens: row.before_tokens,
            after_tokens: row.after_tokens,
            target_tokens: row.target_tokens,
            min_reduction_ppm: row.min_reduction_ppm,
            strategy: row.strategy,
            result,
            error_code: row.error_code,
            error_message: row.error_message,
            created_ms: row.created_ms,
            updated_ms: row.updated_ms,
        })
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

/// The outcome of a compaction attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionRecord {
    /// Durable `compaction` row id — the immutable summary revision a
    /// post-compaction inspector cites (K-04).
    pub id: i64,
    pub accepted: bool,
    pub before: i64,
    pub after: i64,
    pub target: i64,
    pub reduction_ratio: f64,
    /// Present when rejected: why.
    pub reason: Option<String>,
}

impl SessionHandle {
    /// Record a compaction attempt and enforce the hard invariant. Accepted
    /// iff `after <= target` and `reduction >= min_reduction_ratio`; anything
    /// else journals `CompactRejected` and reports the reason. The session
    /// state does not move (compaction is an interior event).
    pub fn record_compaction(
        &self,
        before: i64,
        after: i64,
        target: i64,
        strategy: &str,
        policy: &CompactionPolicy,
    ) -> faktor_core::Result<CompactionRecord> {
        if before <= 0 {
            return Err(SessionError::Malformed(format!(
                "compaction `before` must be > 0, got {before}"
            ))
            .into());
        }
        if after < 0 || target < 0 {
            return Err(SessionError::Malformed(format!(
                "compaction sizes must be >= 0 (before={before}, after={after}, target={target})"
            ))
            .into());
        }
        if after > before {
            return Err(SessionError::Malformed(format!(
                "compaction cannot grow the context: after={after} > before={before}"
            ))
            .into());
        }
        if !(0.0..1.0).contains(&policy.min_reduction_ratio) {
            return Err(SessionError::Malformed(format!(
                "min_reduction_ratio must be in [0, 1), got {}",
                policy.min_reduction_ratio
            ))
            .into());
        }
        if strategy.is_empty() || strategy.len() > 128 {
            return Err(SessionError::Malformed("invalid compaction strategy".into()).into());
        }

        let reduction_ratio = 1.0 - (after as f64 / before as f64);
        let accepted = after <= target && reduction_ratio >= policy.min_reduction_ratio;
        let reason = if !accepted {
            let why = if after > target {
                format!("after ({after}) exceeds target ({target})")
            } else {
                format!(
                    "reduction ({:.3}) below minimum ({:.3})",
                    reduction_ratio, policy.min_reduction_ratio
                )
            };
            Some(why)
        } else {
            None
        };

        let _guard = self.command_guard();
        let kind = if accepted {
            EventKind::ContextCompacted
        } else {
            EventKind::CompactRejected
        };
        // Validate against the pre-state read ONCE; the atomic store command
        // re-verifies it inside the same transaction as the row insert, so a
        // compaction row can never exist without its journal event.
        let current = self.state()?;
        crate::journal::validate_transition(current, kind, current)?;
        let event = crate::ops::command_event(
            kind,
            current,
            None,
            self.now_ms(),
            Some(serde_json::json!({
                "before": before,
                "after": after,
                "target": target,
                "accepted": accepted,
                "strategy": strategy,
                "reduction": reduction_ratio,
                // Truthful degradation surface: a rejected compaction names
                // the exact invariant it missed (target or minimum
                // reduction) instead of only flipping a boolean.
                "reason": reason.clone(),
            })),
        )?;
        let (_seq, id) = self
            .manager
            .store()
            .record_compaction_and_event(
                self.id, before, after, target, accepted, strategy, current, event,
            )
            .map_err(crate::map_store_err)?;
        Ok(CompactionRecord {
            id,
            accepted,
            before,
            after,
            target,
            reduction_ratio,
            reason,
        })
    }

    /// Record a compaction with the default policy.
    pub fn record_compaction_defaults(
        &self,
        before: i64,
        after: i64,
        target: i64,
        strategy: &str,
    ) -> faktor_core::Result<CompactionRecord> {
        self.record_compaction(
            before,
            after,
            target,
            strategy,
            &CompactionPolicy::default(),
        )
    }
}

impl SessionHandle {
    /// Create the durable PREPARED row of one explicit compaction operation.
    /// Replaying the same op id is idempotent; the same id with different
    /// inputs is a typed conflict (never a silent retarget).
    #[allow(clippy::too_many_arguments)]
    pub fn begin_compaction_operation(
        &self,
        operation_id: &str,
        target_tokens: i64,
        min_reduction_ratio: f64,
        expected_revision: Option<u64>,
        manifest_revision: Option<u64>,
    ) -> faktor_core::Result<CompactionOperation> {
        if operation_id.is_empty() || operation_id.len() > 128 {
            return Err(SessionError::Malformed(
                "compaction operation id must be 1..=128 bytes".into(),
            )
            .into());
        }
        if target_tokens < 0 {
            return Err(SessionError::Malformed(format!(
                "compaction target must be >= 0, got {target_tokens}"
            ))
            .into());
        }
        if !(0.0..1.0).contains(&min_reduction_ratio) {
            return Err(SessionError::Malformed(format!(
                "min_reduction_ratio must be in [0, 1), got {min_reduction_ratio}"
            ))
            .into());
        }
        let now = self.now_ms();
        let row = faktor_store::CompactionOperationRow {
            session_id: self.id,
            operation_id: operation_id.to_string(),
            state: CompactionOperationState::Prepared.as_str().into(),
            phase: "prepared".into(),
            progress_ppm: 0,
            expected_revision: expected_revision.and_then(|v| i64::try_from(v).ok()),
            manifest_revision: manifest_revision.and_then(|v| i64::try_from(v).ok()),
            before_tokens: None,
            after_tokens: None,
            target_tokens,
            min_reduction_ppm: (min_reduction_ratio * 1_000_000.0).round() as i64,
            strategy: None,
            result_json: None,
            error_code: None,
            error_message: None,
            created_ms: now,
            updated_ms: now,
        };
        let inserted = self
            .manager
            .store()
            .insert_compaction_operation(row)
            .map_err(crate::map_store_err)?;
        let existing = self
            .compaction_operation(operation_id)?
            .ok_or_else(|| SessionError::Internal("compaction operation vanished".into()))?;
        if !inserted
            && (existing.target_tokens != target_tokens
                || existing.expected_revision != expected_revision
                || existing.min_reduction_ppm != (min_reduction_ratio * 1_000_000.0).round() as i64)
        {
            return Err(SessionError::Conflict(format!(
                "compaction operation {operation_id} already exists with different inputs"
            ))
            .into());
        }
        Ok(existing)
    }

    pub fn compaction_operation(
        &self,
        operation_id: &str,
    ) -> faktor_core::Result<Option<CompactionOperation>> {
        self.manager
            .store()
            .compaction_operation(self.id, operation_id)
            .map_err(crate::map_store_err)?
            .map(CompactionOperation::from_row)
            .transpose()
    }

    pub fn latest_compaction_operation(&self) -> faktor_core::Result<Option<CompactionOperation>> {
        self.manager
            .store()
            .latest_compaction_operation(self.id)
            .map_err(crate::map_store_err)?
            .map(CompactionOperation::from_row)
            .transpose()
    }

    /// Advance a non-terminal operation. A terminal row is immutable and the
    /// update is a no-op returning `None` (a late phase can never rewrite an
    /// applied/refused/cancelled/failed outcome).
    #[allow(clippy::too_many_arguments)]
    pub fn advance_compaction_operation(
        &self,
        operation_id: &str,
        state: CompactionOperationState,
        phase: &str,
        progress_ppm: i64,
        before_tokens: Option<i64>,
        after_tokens: Option<i64>,
        target_tokens: Option<i64>,
        strategy: Option<&str>,
        result: Option<&serde_json::Value>,
        error_code: Option<&str>,
        error_message: Option<&str>,
    ) -> faktor_core::Result<Option<CompactionOperation>> {
        if state.is_terminal() && result.is_none() && error_code.is_none() {
            return Err(SessionError::Malformed(
                "a terminal compaction operation must carry a result or a typed error".into(),
            )
            .into());
        }
        let result_json = match result {
            Some(value) => Some(serde_json::to_string(value).map_err(|e| {
                SessionError::Malformed(format!("compaction result serialization: {e}"))
            })?),
            None => None,
        };
        self.manager
            .store()
            .update_compaction_operation(
                self.id,
                operation_id,
                faktor_store::CompactionOperationUpdate {
                    state: state.as_str(),
                    phase,
                    progress_ppm: progress_ppm.clamp(0, 1_000_000),
                    before_tokens,
                    after_tokens,
                    target_tokens,
                    manifest_revision: None,
                    strategy,
                    result_json: result_json.as_deref(),
                    error_code,
                    error_message,
                },
            )
            .map_err(crate::map_store_err)?
            .map(CompactionOperation::from_row)
            .transpose()
    }

    /// Persist a cancel request. A terminal operation is returned unchanged
    /// so the caller can tell the exact terminal state; `None` means absent.
    pub fn request_compaction_cancel(
        &self,
        operation_id: &str,
    ) -> faktor_core::Result<Option<CompactionOperation>> {
        if let Some(existing) = self.compaction_operation(operation_id)? {
            if existing.is_terminal() {
                return Ok(Some(existing));
            }
        }
        self.manager
            .store()
            .request_compaction_cancel(self.id, operation_id)
            .map_err(crate::map_store_err)?
            .map(CompactionOperation::from_row)
            .transpose()
    }

    /// Bound the durable operation history (terminal rows only).
    pub fn prune_compaction_operations_keep_newest(
        &self,
        keep: usize,
    ) -> faktor_core::Result<usize> {
        let deleted = self
            .manager
            .store()
            .prune_compaction_operations_keep_newest(self.id, keep)
            .map_err(crate::map_store_err)?;
        Ok(deleted)
    }

    /// True when a cancel was requested for a still-running operation.
    pub fn compaction_cancel_requested(&self, operation_id: &str) -> faktor_core::Result<bool> {
        Ok(matches!(
            self.compaction_operation(operation_id)?.map(|op| op.state),
            Some(CompactionOperationState::Cancelling)
        ))
    }

    /// Atomic accepted compaction: the compaction row, the durable
    /// replacement checkpoint and the journal event commit together. The
    /// caller passes the EXACT kept wire turns (serialized) and their
    /// canonical digest.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn commit_compaction_with_checkpoint(
        &self,
        before: i64,
        after: i64,
        target: i64,
        plan_accepted: bool,
        strategy: &str,
        policy: &CompactionPolicy,
        through_seq: u64,
        digest: &str,
        payload: &[u8],
    ) -> faktor_core::Result<CompactionRecord> {
        if before <= 0 {
            return Err(SessionError::Malformed(format!(
                "compaction `before` must be > 0, got {before}"
            ))
            .into());
        }
        if after < 0 || target < 0 {
            return Err(SessionError::Malformed(format!(
                "compaction sizes must be >= 0 (before={before}, after={after}, target={target})"
            ))
            .into());
        }
        if after > before {
            return Err(SessionError::Malformed(format!(
                "compaction cannot grow the context: after={after} > before={before}"
            ))
            .into());
        }
        if !(0.0..1.0).contains(&policy.min_reduction_ratio) {
            return Err(SessionError::Malformed(format!(
                "min_reduction_ratio must be in [0, 1), got {}",
                policy.min_reduction_ratio
            ))
            .into());
        }
        if strategy.is_empty() || strategy.len() > 128 {
            return Err(SessionError::Malformed("invalid compaction strategy".into()).into());
        }
        let reduction_ratio = 1.0 - (after as f64 / before as f64);
        // The COMPACTOR's verdict is authoritative. Size/target arithmetic
        // alone is not proof of a valid plan: a plan that drops the active
        // user instruction (or the verbatim task identity) can have a tiny
        // `after` and would otherwise pass every numeric check while wiping
        // the context. Never reinterpret a rejected plan as success.
        let accepted =
            plan_accepted && after <= target && reduction_ratio >= policy.min_reduction_ratio;
        let reason = if !accepted {
            let why = if !plan_accepted {
                "the compactor rejected the plan (active instruction, identity or target invariant)"
                    .to_string()
            } else if after > target {
                format!("after ({after}) exceeds target ({target})")
            } else {
                format!(
                    "reduction ({:.3}) below minimum ({:.3})",
                    reduction_ratio, policy.min_reduction_ratio
                )
            };
            Some(why)
        } else {
            None
        };
        let _guard = self.command_guard();
        let kind = if accepted {
            EventKind::ContextCompacted
        } else {
            EventKind::CompactRejected
        };
        let current = self.state()?;
        crate::journal::validate_transition(current, kind, current)?;
        let event = crate::ops::command_event(
            kind,
            current,
            None,
            self.now_ms(),
            Some(serde_json::json!({
                "before": before,
                "after": after,
                "target": target,
                "accepted": accepted,
                "strategy": strategy,
                "reduction": reduction_ratio,
                "reason": reason.clone(),
            })),
        )?;
        let id = if accepted {
            let facts = crate::memory::context_checkpoint_facts(through_seq, digest, payload)?;
            let (_seq, id) = self
                .manager
                .store()
                .record_compaction_with_checkpoint_and_event(
                    self.id,
                    before,
                    after,
                    target,
                    true,
                    strategy,
                    current,
                    event,
                    crate::memory::CONTEXT_CHECKPOINT_KIND,
                    &facts,
                )
                .map_err(crate::map_store_err)?;
            id
        } else {
            let (_seq, id) = self
                .manager
                .store()
                .record_compaction_and_event(
                    self.id, before, after, target, false, strategy, current, event,
                )
                .map_err(crate::map_store_err)?;
            id
        };
        Ok(CompactionRecord {
            id,
            accepted,
            before,
            after,
            target,
            reduction_ratio,
            reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use faktor_core::event::EventKind;

    #[test]
    fn compaction_rejects_one_percent_reduction() {
        // The archetypal death-spiral case: 180k -> 178k is "done" by the
        // summarizer but saves ~1%. Reject.
        let (_d, m) = test_manager();
        let s = session(&m);
        let r = s
            .record_compaction_defaults(100_000, 99_000, 99_000, "summarize")
            .unwrap();
        assert!(!r.accepted);
        assert!(r.reason.as_ref().unwrap().contains("minimum"));
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::CompactRejected));
        assert!(!kinds.contains(&EventKind::ContextCompacted));
    }

    #[test]
    fn compaction_operation_lifecycle_is_durable_and_terminal_immutable() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let op = s
            .begin_compaction_operation("op-1", 90, 0.25, Some(7), Some(7))
            .unwrap();
        assert_eq!(op.state, CompactionOperationState::Prepared);
        assert_eq!(op.expected_revision, Some(7));
        // Idempotent replay with the same inputs.
        let again = s
            .begin_compaction_operation("op-1", 90, 0.25, Some(7), Some(7))
            .unwrap();
        assert_eq!(again, op);
        // Different inputs on the same id are a typed conflict.
        assert!(s
            .begin_compaction_operation("op-1", 10, 0.25, Some(7), Some(7))
            .is_err());
        let advanced = s
            .advance_compaction_operation(
                "op-1",
                CompactionOperationState::Reducing,
                "reducing-history",
                400_000,
                Some(100),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(advanced.progress_ppm, 400_000);
        let applied = s
            .advance_compaction_operation(
                "op-1",
                CompactionOperationState::Applied,
                "applied",
                1_000_000,
                Some(100),
                Some(40),
                Some(75),
                Some("deterministic_pruning"),
                Some(&serde_json::json!({"accepted": true})),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        assert!(applied.is_terminal());
        // A late phase write cannot rewrite the terminal outcome.
        assert!(s
            .advance_compaction_operation(
                "op-1",
                CompactionOperationState::Failed,
                "failed",
                0,
                None,
                None,
                None,
                None,
                None,
                Some("late"),
                Some("late"),
            )
            .unwrap()
            .is_none());
        assert_eq!(s.latest_compaction_operation().unwrap().unwrap(), applied);
        // Reopen the manager on the same store: the operation survives.
        let reopened = crate::SessionManager::open(
            m.store().root().to_path_buf(),
            m.store().root().parent().unwrap().join("cas-reopen"),
            false,
        )
        .unwrap();
        let rh = reopened.get_session(s.id).unwrap().unwrap();
        assert_eq!(rh.compaction_operation("op-1").unwrap().unwrap(), applied);
    }

    #[test]
    fn compaction_cancel_is_durable_before_commit() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.begin_compaction_operation("op-c", 0, 0.25, None, None)
            .unwrap();
        let cancelling = s.request_compaction_cancel("op-c").unwrap().unwrap();
        assert_eq!(cancelling.state, CompactionOperationState::Cancelling);
        assert!(s.compaction_cancel_requested("op-c").unwrap());
        let cancelled = s
            .advance_compaction_operation(
                "op-c",
                CompactionOperationState::Cancelled,
                "cancelled",
                1_000_000,
                None,
                None,
                None,
                None,
                Some(&serde_json::json!({"cancelled": true})),
                Some("cancelled"),
                Some("cancelled before commit"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(cancelled.state, CompactionOperationState::Cancelled);
        // A second cancel request returns the terminal row unchanged.
        assert_eq!(
            s.request_compaction_cancel("op-c").unwrap().unwrap(),
            cancelled
        );
    }

    #[test]
    fn atomic_compaction_commit_writes_checkpoint_and_event_together() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let payload = br#"[{"role":"user","text":"KEEP-ME"}]"#.to_vec();
        let digest = blake3::hash(&payload).to_hex().to_string();
        let record = s
            .commit_compaction_with_checkpoint(
                100,
                50,
                60,
                true,
                "deterministic_pruning",
                &CompactionPolicy {
                    min_reduction_ratio: 0.25,
                },
                41,
                &digest,
                &payload,
            )
            .unwrap();
        assert!(record.accepted);
        assert!(record.id > 0);
        let checkpoint = s.context_checkpoint().unwrap().unwrap();
        assert_eq!(checkpoint.through_seq, 41);
        assert_eq!(checkpoint.digest, digest);
        assert_eq!(checkpoint.payload, payload);
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::ContextCompacted));
    }

    #[test]
    fn atomic_compaction_refusal_writes_no_checkpoint() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let record = s
            .commit_compaction_with_checkpoint(
                100_000,
                99_000,
                99_000,
                true,
                "deterministic_pruning",
                &CompactionPolicy {
                    min_reduction_ratio: 0.25,
                },
                9,
                &"0".repeat(64),
                b"[]",
            )
            .unwrap();
        assert!(!record.accepted);
        assert!(s.context_checkpoint().unwrap().is_none());
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::CompactRejected));
        assert!(!kinds.contains(&EventKind::ContextCompacted));
    }

    /// Regression: the numeric target/reduction checks alone must never
    /// turn a REJECTED plan into an applied compaction. A plan can have a
    /// tiny `after` by dropping the active instruction; only the compactor
    /// (which proves the newest user turn/identity survives) may accept it.
    #[test]
    fn a_rejected_plan_is_never_reinterpreted_as_an_applied_compaction() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let record = s
            .commit_compaction_with_checkpoint(
                100,
                10,
                20,
                false,
                "deterministic_pruning",
                &CompactionPolicy {
                    min_reduction_ratio: 0.25,
                },
                7,
                &"1".repeat(64),
                b"[]",
            )
            .unwrap();
        assert!(!record.accepted, "a rejected plan must stay rejected");
        assert!(record
            .reason
            .as_deref()
            .unwrap()
            .contains("compactor rejected"));
        assert!(s.context_checkpoint().unwrap().is_none());
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::CompactRejected));
        assert!(!kinds.contains(&EventKind::ContextCompacted));
    }

    #[test]
    fn compaction_accepts_meaningful_reduction() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let r = s
            .record_compaction_defaults(100_000, 60_000, 90_000, "deterministic_prune")
            .unwrap();
        assert!(r.accepted);
        assert!(r.reason.is_none());
        assert!((r.reduction_ratio - 0.4).abs() < 1e-9);
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::ContextCompacted));
    }

    #[test]
    fn compaction_after_exceeding_target_is_rejected() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let r = s
            .record_compaction_defaults(100_000, 95_000, 90_000, "summarize")
            .unwrap();
        assert!(!r.accepted);
        assert!(r.reason.as_ref().unwrap().contains("target"));
        // A summary that *grows* context is malformed, not just rejected.
        assert!(s
            .record_compaction_defaults(100_000, 120_000, 90_000, "x")
            .is_err());
        assert!(s.record_compaction_defaults(0, 0, 0, "x").is_err());
        assert!(s.record_compaction_defaults(-5, 0, 0, "x").is_err());
    }

    #[test]
    fn compaction_hard_invariant_rejects_zero_reduction_even_at_target() {
        // before == target, after == before: nothing to compact.
        let (_d, m) = test_manager();
        let s = session(&m);
        let r = s
            .record_compaction_defaults(90_000, 90_000, 90_000, "x")
            .unwrap();
        assert!(!r.accepted);
        assert!((r.reduction_ratio).abs() < 1e-12);
    }

    #[test]
    fn compaction_policy_is_configurable_and_validated() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let lax = CompactionPolicy {
            min_reduction_ratio: 0.01,
        };
        // With a 1% floor, a 5% reduction is accepted.
        let r = s
            .record_compaction(100_000, 95_000, 95_000, "x", &lax)
            .unwrap();
        assert!(r.accepted);
        // An insane floor is malformed.
        assert!(s
            .record_compaction(
                100_000,
                95_000,
                95_000,
                "x",
                &CompactionPolicy {
                    min_reduction_ratio: 1.5
                }
            )
            .is_err());
        assert!(s
            .record_compaction(
                100_000,
                95_000,
                95_000,
                "x",
                &CompactionPolicy {
                    min_reduction_ratio: -0.1
                }
            )
            .is_err());
    }

    #[test]
    fn compaction_events_are_interior_and_state_preserving() {
        let (_d, m) = test_manager();
        let s = session(&m);
        s.submit_prompt("x", &[]).unwrap();
        s.record_compaction_defaults(100_000, 50_000, 80_000, "prune")
            .unwrap();
        assert_eq!(
            s.state().unwrap(),
            faktor_core::state::AgentState::Preparing
        );
        assert_eq!(
            s.replay_journal().unwrap().state,
            faktor_core::state::AgentState::Preparing
        );
    }

    #[test]
    fn compaction_seams_reopen_old_or_new_only() {
        // One compaction command = ONE transaction: a crash at any durability
        // boundary reopens on exactly the old world (no event) or exactly the
        // new one (row and event together).
        for seam in [
            "session_command_side_row",
            "session_command_precommit",
            "session_command_committed",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = session(&m);
            let sid = s.id();
            m.store().crash_arm(faktor_store::CrashArm {
                point: seam,
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = s.record_compaction_defaults(100_000, 40_000, 50_000, "summarize");
            }));
            assert!(
                m.store().seam_crash_observed(&caught),
                "seam {seam} must fire"
            );
            drop(s);
            drop(m);
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = m.get_session(sid).unwrap().unwrap();
            let compacted = s
                .events_range(1, None)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::ContextCompacted)
                .count();
            match seam {
                "session_command_committed" => {
                    assert_eq!(compacted, 1, "committed compaction journals once");
                }
                _ => {
                    assert_eq!(compacted, 0, "rolled-back compaction leaves no event");
                }
            }
            // Compaction is interior: the state is preserved either way.
            assert_eq!(s.state().unwrap(), faktor_core::state::AgentState::Idle);
        }
    }
}
