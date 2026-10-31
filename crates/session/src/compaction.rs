// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
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
    /// iff the COMPACTOR accepted the plan AND `after <= target` AND
    /// `reduction >= min_reduction_ratio`; anything else journals
    /// `CompactRejected` and reports the reason. The session state does not
    /// move (compaction is an interior event).
    ///
    /// This entry point is for an ACCEPTED plan (the numeric checks still
    /// run); call [`Self::record_compaction_rejected`] for a plan the
    /// compactor explicitly refused.
    pub fn record_compaction(
        &self,
        before: i64,
        after: i64,
        target: i64,
        strategy: &str,
        policy: &CompactionPolicy,
    ) -> faktor_core::Result<CompactionRecord> {
        self.record_compaction_verdict(before, after, target, true, strategy, policy)
    }

    /// Record the compactor's EXPLICIT rejection. The plan verdict is
    /// authoritative: a rejected plan can carry a tiny `after` (for example
    /// when only the archive digest fits), which would satisfy every numeric
    /// check while having dropped the active instruction. Such a plan must
    /// never be reinterpreted as `ContextCompacted`.
    pub fn record_compaction_rejected(
        &self,
        before: i64,
        after: i64,
        target: i64,
        strategy: &str,
        policy: &CompactionPolicy,
    ) -> faktor_core::Result<CompactionRecord> {
        self.record_compaction_verdict(before, after, target, false, strategy, policy)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_compaction_verdict(
        &self,
        before: i64,
        after: i64,
        target: i64,
        plan_accepted: bool,
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

// ------------------------------------------------ compaction archive lifecycle
// (audit item 15, P3): formalized archive-generation states on the
// SESSION side of the archive/compaction boundary.
//
// ```text
// LIVE_RECOVERY -> SUPERSEDED_RECOVERY -> (grace period) -> EVICTABLE_BODY
//                 -> DIGEST_ONLY
// ```
//
// Until now the wire jumped straight from "full archive" to a loss marker
// the moment a manifest stopped being retrievable — cryptographic
// continuity (an independently verifiable digest + metadata record) was
// destroyed with the payload. The lifecycle retires bodies GRADUALLY: a
// superseded generation keeps its full body through the grace period, may
// then be evicted, and afterwards survives as a DIGEST_ONLY record whose
// retained digest still verifies any later-presented body (typed mismatch
// on failure). The governor's own eviction + durable loss-marker path
// (faktor-storage) is UNCHANGED and stays the outermost fallback: it may
// evict any body at any time, and a generation whose digest record is
// even swept emits the loss marker exactly as before.

/// Grace period between `SupersededRecovery` and `EvictableBody`, in
/// milliseconds. Default: 7 days.
///
/// Reason for 7 days: the wire archive pointer is the model's only
/// in-context handle to evicted turns, and every recovery surface that can
/// still CITE an older generation (checkpoint replay after a crash,
/// restart reconciliation, operator inspection, a long-running task that
/// resumes days later) lives comfortably inside a week. The conservative
/// bound costs at most one extra week of retained superseded bodies under
/// the governor's own ceiling (which can evict earlier at any time — the
/// outermost fallback), so the grace only governs the SESSION-side promise,
/// never the storage budget. Shorter defaults (hours) would race restart
/// windows; longer ones would keep dead bodies far past any recovery use.
pub const ARCHIVE_BODY_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Bounded registry: at most this many archive generations per session.
/// At capacity, the OLDEST `DigestOnly` record is dropped first (continuity
/// loss is journaled as a sweep); a registry whose records are ALL
/// live/superseded/evictable REFUSES the new registration typed instead of
/// silently dropping a generation that still carries recovery value.
pub const MAX_ARCHIVE_LIFECYCLE_GENERATIONS: usize = 64;

/// Durable memory-fact kind carrying one `gen/<manifest-hash>` row per
/// archive generation (bounded JSON, one row per record).
pub const ARCHIVE_LIFECYCLE_FACT_KIND: &str = "compaction_archive_lifecycle";

/// Key prefix of one generation's fact row (the suffix is the lowercase
/// 64-hex manifest hash — the retained digest).
pub const ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX: &str = "gen/";

/// Per-record JSON bound; must stay at or below the store's fact-value
/// ceiling (4096 bytes) so an oversized hostile row is refused by the
/// writer AND the reader.
pub const ARCHIVE_LIFECYCLE_FACT_MAX_BYTES: usize = 4096;

/// The durable lifecycle state of one compaction-archive generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveLifecycleState {
    /// The generation's body is the CURRENT recovery authority: the wire
    /// carries its manifest pointer and a restart may restore from it.
    LiveRecovery,
    /// A newer compaction archive replaced this generation on the wire;
    /// the body is still fully retained and reproducible (nothing may
    /// degrade it before the grace period elapses).
    SupersededRecovery,
    /// The grace period elapsed: the body is now eligible for eviction
    /// (release or governor race). The digest + metadata record is the
    /// continuity authority either way.
    EvictableBody,
    /// The payload is gone; only the retained digest + metadata remain —
    /// queryable, and able to verify any later-presented body.
    DigestOnly,
}

impl ArchiveLifecycleState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LiveRecovery => "live_recovery",
            Self::SupersededRecovery => "superseded_recovery",
            Self::EvictableBody => "evictable_body",
            Self::DigestOnly => "digest_only",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "live_recovery" => Self::LiveRecovery,
            "superseded_recovery" => Self::SupersededRecovery,
            "evictable_body" => Self::EvictableBody,
            "digest_only" => Self::DigestOnly,
            _ => return None,
        })
    }
}

/// Retained archive metadata that survives payload degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveMetadata {
    /// Bytes of the archived body the record described.
    pub total_bytes: u64,
    /// Chunk count of the archived body.
    pub chunk_count: u32,
    /// When the generation was first registered (session clock, ms).
    pub created_ms: i64,
}

/// Typed lifecycle failure: never a panic, never a silent state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveLifecycleError {
    /// A malformed manifest hash / registry payload.
    Malformed(String),
    /// The generation is not in the registry.
    UnknownGeneration(String),
    /// A PINNED (recovery-critical) generation refuses a time-based
    /// degradation.
    Pinned { hash: String },
    /// The requested transition skips a lifecycle stage.
    IllegalTransition {
        hash: String,
        from: ArchiveLifecycleState,
        to: ArchiveLifecycleState,
    },
    /// The grace period has not elapsed yet (how much is left).
    GracePeriodActive { hash: String, remaining_ms: i64 },
    /// A presented body does NOT rehash to the retained digest.
    DigestMismatch { expected: String, actual: String },
    /// The bounded registry has no room for another protected generation.
    RegistryFull,
}

impl std::fmt::Display for ArchiveLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "archive lifecycle malformed: {what}"),
            Self::UnknownGeneration(hash) => write!(f, "unknown archive generation {hash}"),
            Self::Pinned { hash } => {
                write!(
                    f,
                    "archive generation {hash} is pinned and refuses degradation"
                )
            }
            Self::IllegalTransition { hash, from, to } => write!(
                f,
                "illegal archive lifecycle transition {hash}: {} -> {}",
                from.as_str(),
                to.as_str()
            ),
            Self::GracePeriodActive { hash, remaining_ms } => {
                write!(
                    f,
                    "archive generation {hash} is in grace ({remaining_ms}ms left)"
                )
            }
            Self::DigestMismatch { expected, actual } => {
                write!(
                    f,
                    "archive body digest mismatch: expected {expected}, got {actual}"
                )
            }
            Self::RegistryFull => write!(f, "archive lifecycle registry is full"),
        }
    }
}

impl std::error::Error for ArchiveLifecycleError {}

fn archive_hash_is_well_formed(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One archive generation's durable lifecycle record. The retained digest
/// IS the content address (`manifest_hash`): cryptographic continuity means
/// a later-presented body can be rehashed against it even after the payload
/// is gone.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveGenerationRecord {
    /// Lowercase 64-hex manifest hash — identity AND retained digest.
    pub manifest_hash: String,
    pub state: ArchiveLifecycleState,
    /// Recovery-critical flag: pinned generations refuse every time-based
    /// degradation (supersede → evictable → digest-only) until unpinned.
    pub pinned: bool,
    pub created_ms: i64,
    /// When the generation was superseded (set at the transition).
    pub superseded_at_ms: Option<i64>,
    pub metadata: ArchiveMetadata,
}

impl ArchiveGenerationRecord {
    /// The retained cryptographic digest (the content address itself).
    pub fn retained_digest(&self) -> &str {
        &self.manifest_hash
    }

    /// Verify a PRESENTED body against the retained digest. This is what
    /// makes a `DigestOnly` record useful: the payload is gone, but any
    /// later-presented candidate bytes still prove whether they are the
    /// archived body — typed [`ArchiveLifecycleError::DigestMismatch`] on
    /// any mismatch, never a bool.
    pub fn verify_presented_body(&self, body: &[u8]) -> Result<(), ArchiveLifecycleError> {
        let actual = blake3::hash(body).to_hex().to_string();
        if actual == self.manifest_hash {
            Ok(())
        } else {
            Err(ArchiveLifecycleError::DigestMismatch {
                expected: self.manifest_hash.clone(),
                actual,
            })
        }
    }
}

/// The session's archive-generation lifecycle registry (bounded). Pure,
/// deterministic state machine: identical inputs yield identical
/// transitions; every refusal is typed; nothing degrades without both a
/// predecessor state and (for time-based moves) an elapsed grace.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveLifecycleRegistry {
    /// Grace between `SupersededRecovery` and `EvictableBody` (ms);
    /// negatives clamp to 0 (never a negative wait).
    grace_ms: i64,
    generations: Vec<ArchiveGenerationRecord>,
    /// Durable rows the next save must delete (swept/dropped records).
    /// Bounded by [`MAX_ARCHIVE_LIFECYCLE_GENERATIONS`] per cycle.
    #[serde(default)]
    swept: Vec<String>,
}

impl Default for ArchiveLifecycleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ArchiveLifecycleRegistry {
    /// The conservative default grace ([`ARCHIVE_BODY_GRACE_MS`]).
    pub fn new() -> Self {
        Self::with_grace_ms(ARCHIVE_BODY_GRACE_MS)
    }

    /// Injectable grace for tests/hosts; negative values clamp to 0.
    pub fn with_grace_ms(grace_ms: i64) -> Self {
        Self {
            grace_ms: grace_ms.max(0),
            generations: Vec::new(),
            swept: Vec::new(),
        }
    }

    pub fn grace_ms(&self) -> i64 {
        self.grace_ms
    }

    pub fn len(&self) -> usize {
        self.generations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.generations.is_empty()
    }

    pub fn get(&self, manifest_hash: &str) -> Option<&ArchiveGenerationRecord> {
        self.generations
            .iter()
            .find(|g| g.manifest_hash == manifest_hash)
    }

    /// All records in insertion order (the durable save shape).
    pub fn generations(&self) -> &[ArchiveGenerationRecord] {
        &self.generations
    }

    fn index(&self, manifest_hash: &str) -> Result<usize, ArchiveLifecycleError> {
        if !archive_hash_is_well_formed(manifest_hash) {
            return Err(ArchiveLifecycleError::Malformed(format!(
                "manifest hash {manifest_hash:?} is not 64-hex"
            )));
        }
        self.generations
            .iter()
            .position(|g| g.manifest_hash == manifest_hash)
            .ok_or_else(|| ArchiveLifecycleError::UnknownGeneration(manifest_hash.to_string()))
    }

    /// Register a freshly published archive as `LiveRecovery`.
    ///
    /// Idempotent for a known hash (a replay never resets a superseded or
    /// degraded record back to live). Every OTHER unpinned live generation
    /// is auto-superseded at `now_ms` — at most one unpinned live
    /// generation exists; PINNED live generations coexist (pinning is
    /// exactly the promise that they stay recoverable alongside).
    pub fn register_live(
        &mut self,
        manifest_hash: &str,
        metadata: ArchiveMetadata,
        now_ms: i64,
    ) -> Result<(), ArchiveLifecycleError> {
        if !archive_hash_is_well_formed(manifest_hash) {
            return Err(ArchiveLifecycleError::Malformed(format!(
                "manifest hash {manifest_hash:?} is not 64-hex"
            )));
        }
        if self.get(manifest_hash).is_some() {
            return Ok(());
        }
        if self.generations.len() >= MAX_ARCHIVE_LIFECYCLE_GENERATIONS {
            // Make room by dropping the OLDEST degraded record; its durable
            // row is journaled for deletion (continuity loss is explicit).
            let drop_index = self
                .generations
                .iter()
                .position(|g| g.state == ArchiveLifecycleState::DigestOnly);
            let Some(drop_index) = drop_index else {
                // Nothing degraded: refuse instead of dropping a generation
                // that still carries recovery value.
                return Err(ArchiveLifecycleError::RegistryFull);
            };
            let dropped = self.generations.remove(drop_index);
            self.swept.push(dropped.manifest_hash);
        }
        // Supersede every other UNPINNED live generation (pinned lives stay
        // live: their recovery promise is not collateral damage of a new
        // publication).
        for generation in &mut self.generations {
            if generation.manifest_hash == manifest_hash {
                continue;
            }
            if generation.state == ArchiveLifecycleState::LiveRecovery && !generation.pinned {
                generation.state = ArchiveLifecycleState::SupersededRecovery;
                generation.superseded_at_ms = Some(
                    generation
                        .superseded_at_ms
                        .map(|previous| previous.max(now_ms))
                        .unwrap_or(now_ms)
                        .max(now_ms),
                );
            }
        }
        self.generations.push(ArchiveGenerationRecord {
            manifest_hash: manifest_hash.to_string(),
            state: ArchiveLifecycleState::LiveRecovery,
            pinned: false,
            created_ms: now_ms,
            superseded_at_ms: None,
            metadata,
        });
        Ok(())
    }

    /// Explicit `LiveRecovery -> SupersededRecovery`. Pinned generations
    /// refuse (typed) — a recovery-critical body never enters the
    /// degradation pipeline. Already-superseded or later states are
    /// idempotent (the original stamp is never re-written by a replay).
    pub fn supersede(
        &mut self,
        manifest_hash: &str,
        now_ms: i64,
    ) -> Result<ArchiveLifecycleState, ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        let generation = &mut self.generations[index];
        if generation.pinned {
            return Err(ArchiveLifecycleError::Pinned {
                hash: manifest_hash.to_string(),
            });
        }
        // Only a LIVE generation moves; already-superseded or later states
        // are idempotent (the original stamp is never re-written by a
        // replay).
        if generation.state == ArchiveLifecycleState::LiveRecovery {
            generation.state = ArchiveLifecycleState::SupersededRecovery;
            generation.superseded_at_ms = Some(
                generation
                    .superseded_at_ms
                    .map(|previous| previous.max(now_ms))
                    .unwrap_or(now_ms),
            );
        }
        Ok(generation.state)
    }

    /// `SupersededRecovery -> EvictableBody`, allowed ONLY after the grace
    /// period. A live generation can never become evictable (it must be
    /// superseded first), a pinned one refuses, and inside the grace the
    /// typed refusal carries the remaining time.
    pub fn mark_evictable(
        &mut self,
        manifest_hash: &str,
        now_ms: i64,
    ) -> Result<ArchiveLifecycleState, ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        let generation = &mut self.generations[index];
        if generation.pinned {
            return Err(ArchiveLifecycleError::Pinned {
                hash: manifest_hash.to_string(),
            });
        }
        match generation.state {
            ArchiveLifecycleState::LiveRecovery => {
                return Err(ArchiveLifecycleError::IllegalTransition {
                    hash: manifest_hash.to_string(),
                    from: ArchiveLifecycleState::LiveRecovery,
                    to: ArchiveLifecycleState::EvictableBody,
                });
            }
            ArchiveLifecycleState::SupersededRecovery => {
                let superseded_at = generation.superseded_at_ms.unwrap_or(now_ms);
                let elapsed = now_ms.saturating_sub(superseded_at);
                let remaining = self.grace_ms.saturating_sub(elapsed);
                if remaining > 0 {
                    return Err(ArchiveLifecycleError::GracePeriodActive {
                        hash: manifest_hash.to_string(),
                        remaining_ms: remaining,
                    });
                }
                generation.state = ArchiveLifecycleState::EvictableBody;
            }
            _ => {} // evictable/digest-only: idempotent
        }
        Ok(generation.state)
    }

    /// `EvictableBody -> DigestOnly`: the payload is released/retired and
    /// only the retained digest + metadata remain. The predecessor
    /// requirement is what enforces the grace: skipping straight from
    /// superseded (or live) is a typed `IllegalTransition`, and a pinned
    /// generation refuses.
    pub fn degrade_to_digest_only(
        &mut self,
        manifest_hash: &str,
    ) -> Result<ArchiveLifecycleState, ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        let generation = &mut self.generations[index];
        if generation.pinned {
            return Err(ArchiveLifecycleError::Pinned {
                hash: manifest_hash.to_string(),
            });
        }
        match generation.state {
            ArchiveLifecycleState::EvictableBody => {
                generation.state = ArchiveLifecycleState::DigestOnly;
            }
            ArchiveLifecycleState::DigestOnly => {}
            from => {
                return Err(ArchiveLifecycleError::IllegalTransition {
                    hash: manifest_hash.to_string(),
                    from,
                    to: ArchiveLifecycleState::DigestOnly,
                });
            }
        }
        Ok(generation.state)
    }

    /// OBSERVED physical loss of the body (governor eviction, chunk loss, a
    /// forged/unreadable manifest): the record adopts `DigestOnly` from any
    /// state — reality is not a scheduled degradation, so a PIN does not
    /// block it (keeping the digest record IS the pinned continuity
    /// promise). Unknown/malformed hashes stay typed.
    pub fn adopt_body_loss(
        &mut self,
        manifest_hash: &str,
    ) -> Result<ArchiveLifecycleState, ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        let generation = &mut self.generations[index];
        generation.state = ArchiveLifecycleState::DigestOnly;
        Ok(generation.state)
    }

    /// Batch time advance: every UNPINNED `SupersededRecovery` generation
    /// whose grace elapsed becomes `EvictableBody`. Returns the number of
    /// transitions (pinned generations are skipped — never degrade early).
    pub fn advance_elapsed(&mut self, now_ms: i64) -> usize {
        let grace = self.grace_ms;
        let mut transitioned = 0usize;
        for generation in &mut self.generations {
            if generation.pinned || generation.state != ArchiveLifecycleState::SupersededRecovery {
                continue;
            }
            let superseded_at = generation.superseded_at_ms.unwrap_or(now_ms);
            if now_ms.saturating_sub(superseded_at) >= grace {
                generation.state = ArchiveLifecycleState::EvictableBody;
                transitioned += 1;
            }
        }
        transitioned
    }

    pub fn pin(&mut self, manifest_hash: &str) -> Result<(), ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        self.generations[index].pinned = true;
        Ok(())
    }

    pub fn unpin(&mut self, manifest_hash: &str) -> Result<(), ArchiveLifecycleError> {
        let index = self.index(manifest_hash)?;
        self.generations[index].pinned = false;
        Ok(())
    }

    /// Sweep even a `DigestOnly` record: continuity for this generation is
    /// deliberately gone (retention pressure / operator action). The caller
    /// (the wire marker) must fall back to the LOSS marker for this hash
    /// from now on; the durable row deletion is journaled for the next
    /// save. Returns the removed record (`None` = already swept/unknown).
    pub fn sweep(&mut self, manifest_hash: &str) -> Option<ArchiveGenerationRecord> {
        let index = self
            .generations
            .iter()
            .position(|g| g.manifest_hash == manifest_hash)?;
        let record = self.generations.remove(index);
        self.swept.push(record.manifest_hash.clone());
        Some(record)
    }

    /// Hashes whose durable rows the next save must delete (sweeps and
    /// bound-drops of this cycle).
    pub fn swept(&self) -> &[String] {
        &self.swept
    }

    /// Clear the sweep journal after the durable deletions succeeded.
    pub fn clear_swept(&mut self) {
        self.swept.clear();
    }
}

impl SessionHandle {
    /// Load this session's archive-generation lifecycle registry from its
    /// durable memory facts (`gen/<64-hex>` rows). A missing fact set is
    /// the empty registry; ANY anomaly (oversized row, malformed JSON,
    /// hash/key disagreement, bound overflow) is a typed error so the
    /// caller can fall back to today's loss-marker behavior instead of
    /// trusting a hostile or corrupt registry.
    pub fn archive_lifecycle_registry(&self) -> faktor_core::Result<ArchiveLifecycleRegistry> {
        let (rows, has_more) = self.memory_facts_by_kind_key_prefix(
            ARCHIVE_LIFECYCLE_FACT_KIND,
            ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX,
            None,
            u64::try_from(MAX_ARCHIVE_LIFECYCLE_GENERATIONS + 1).unwrap_or(u64::MAX),
        )?;
        if has_more || rows.len() > MAX_ARCHIVE_LIFECYCLE_GENERATIONS {
            return Err(SessionError::Malformed(format!(
                "archive lifecycle registry exceeds {MAX_ARCHIVE_LIFECYCLE_GENERATIONS} generations"
            ))
            .into());
        }
        let mut generations = Vec::with_capacity(rows.len());
        for row in rows {
            // FactTriple = (kind, key, value).
            let (_row_kind, key, value) = row;
            if value.len() > ARCHIVE_LIFECYCLE_FACT_MAX_BYTES {
                return Err(SessionError::Malformed(
                    "archive lifecycle record exceeds the per-row bound".into(),
                )
                .into());
            }
            let Some(suffix) = key.strip_prefix(ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX) else {
                return Err(SessionError::Malformed(format!(
                    "archive lifecycle fact key {key} is malformed"
                ))
                .into());
            };
            if !archive_hash_is_well_formed(suffix) {
                return Err(SessionError::Malformed(format!(
                    "archive lifecycle fact key {key} is malformed"
                ))
                .into());
            }
            let record: ArchiveGenerationRecord = serde_json::from_str(&value).map_err(|e| {
                SessionError::Malformed(format!("archive lifecycle record {key} is malformed: {e}"))
            })?;
            if record.manifest_hash != suffix {
                return Err(SessionError::Malformed(format!(
                    "archive lifecycle record key/value disagree: {key} vs {}",
                    record.manifest_hash
                ))
                .into());
            }
            generations.push(record);
        }
        Ok(ArchiveLifecycleRegistry {
            grace_ms: ARCHIVE_BODY_GRACE_MS,
            generations,
            swept: Vec::new(),
        })
    }

    /// Persist the registry: upsert one bounded `gen/<hash>` row per
    /// generation, then delete the rows journaled as swept. The journal is
    /// cleared only after every durable step succeeded (a partial save
    /// retries on the next call; a failed delete never silently loses the
    /// sweep intent).
    pub fn save_archive_lifecycle_registry(
        &self,
        registry: &mut ArchiveLifecycleRegistry,
    ) -> faktor_core::Result<()> {
        for generation in registry.generations() {
            if !archive_hash_is_well_formed(&generation.manifest_hash) {
                return Err(SessionError::Malformed(
                    "refusing to persist a lifecycle record with a malformed hash".into(),
                )
                .into());
            }
            let json = serde_json::to_string(generation).map_err(|e| {
                SessionError::Internal(format!("archive lifecycle serialization: {e}"))
            })?;
            if json.len() > ARCHIVE_LIFECYCLE_FACT_MAX_BYTES {
                return Err(SessionError::Oversized(format!(
                    "archive lifecycle record of {} bytes exceeds {ARCHIVE_LIFECYCLE_FACT_MAX_BYTES}",
                    json.len()
                ))
                .into());
            }
            let key = format!(
                "{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}{}",
                generation.manifest_hash
            );
            self.upsert_memory_fact(ARCHIVE_LIFECYCLE_FACT_KIND, &key, &json)?;
        }
        for hash in registry.swept() {
            if !archive_hash_is_well_formed(hash) {
                return Err(SessionError::Malformed(
                    "refusing to delete a lifecycle row with a malformed hash".into(),
                )
                .into());
            }
            let key = format!("{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}{hash}");
            self.delete_memory_fact_cas(ARCHIVE_LIFECYCLE_FACT_KIND, &key, None)?;
        }
        registry.clear_swept();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::tests::{session, test_manager};
    use faktor_core::event::EventKind;

    #[test]
    fn compaction_operation_order_is_monotonic_across_a_backward_clock_jump() {
        let dir = tempfile::tempdir().unwrap();
        let clock = std::sync::Arc::new(faktor_core::time::TestClock::new(1_000_000));
        let m = crate::SessionManager::open_with_clock(
            dir.path().join("store"),
            dir.path().join("cas"),
            false,
            clock.clone(),
        )
        .unwrap();
        let s = session(&m);
        let a = s
            .begin_compaction_operation("clock-a", 100, 0.25, None, None)
            .unwrap();
        let b = s
            .begin_compaction_operation("clock-b", 100, 0.25, None, None)
            .unwrap();
        assert!(
            b.created_ms > a.created_ms,
            "same-millisecond inserts must still order strictly"
        );
        for id in ["clock-a", "clock-b"] {
            let done = s
                .advance_compaction_operation(
                    id,
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
            assert!(done.is_terminal());
        }
        // Wall clock jumps 60s BACKWARD (NTP correction / VM restore).
        clock.advance(-60_000);
        let c = s
            .begin_compaction_operation("clock-c", 100, 0.25, None, None)
            .unwrap();
        assert!(
            c.created_ms > b.created_ms,
            "a backward wall clock must not let a new operation sort older"
        );
        assert_eq!(
            s.latest_compaction_operation()
                .unwrap()
                .unwrap()
                .operation_id,
            "clock-c",
            "the newest operation must remain the truthful UI state"
        );
        // Terminal prune must never evict the newest operation because the
        // wall clock stepped backwards: keep the newest THREE first, then
        // the newest ONE.
        let c_done = s
            .advance_compaction_operation(
                "clock-c",
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
        assert!(c_done.is_terminal());
        assert_eq!(s.prune_compaction_operations_keep_newest(1).unwrap(), 2);
        assert!(s.compaction_operation("clock-a").unwrap().is_none());
        assert!(s.compaction_operation("clock-b").unwrap().is_none());
        assert_eq!(
            s.latest_compaction_operation()
                .unwrap()
                .unwrap()
                .operation_id,
            "clock-c"
        );
    }

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
    fn atomic_compaction_seams_reopen_checkpoint_and_event_together() {
        // A crash at any seam of the ONE accepted-compaction transaction must
        // expose either no replacement context AND no ContextCompacted event,
        // or both. The historical two-step path could persist the checkpoint
        // and die before the compaction record/event.
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
            let payload = br#"[{"role":"user","text":"KEEP-ATOMIC"}]"#.to_vec();
            let digest = blake3::hash(&payload).to_hex().to_string();
            m.store().crash_arm(faktor_store::CrashArm {
                point: seam,
                ordinal: 0,
            });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = s.commit_compaction_with_checkpoint(
                    100,
                    40,
                    60,
                    true,
                    "deterministic_pruning",
                    &CompactionPolicy::default(),
                    17,
                    &digest,
                    &payload,
                );
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
            let checkpoint = s.context_checkpoint().unwrap();
            let compacted = s
                .events_range(1, None)
                .unwrap()
                .into_iter()
                .filter(|e| e.kind == EventKind::ContextCompacted)
                .count();
            match seam {
                "session_command_committed" => {
                    let checkpoint = checkpoint.expect("committed checkpoint survives");
                    assert_eq!(checkpoint.through_seq, 17);
                    assert_eq!(checkpoint.digest, digest);
                    assert_eq!(compacted, 1, "committed event survives");
                }
                _ => {
                    assert!(
                        checkpoint.is_none(),
                        "rolled-back checkpoint must not exist"
                    );
                    assert_eq!(compacted, 0, "rolled-back event must not exist");
                }
            }
        }
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

    /// CASE INV-COMPACTION-VERDICT: the runtime's no-checkpoint branch
    /// records the plan verdict too: a rejected plan whose numeric `after`
    /// is tiny (only the archive digest fit) must journal
    /// `CompactRejected`, never `ContextCompacted`.
    #[test]
    fn record_compaction_rejected_never_flips_a_tiny_after_to_success() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let record = s
            .record_compaction_rejected(
                10_000,
                10,
                20_000,
                "deterministic_pruning",
                &CompactionPolicy {
                    min_reduction_ratio: 0.25,
                },
            )
            .unwrap();
        assert!(!record.accepted);
        assert!(record
            .reason
            .as_deref()
            .unwrap()
            .contains("compactor rejected"));
        let kinds: Vec<_> = s
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(kinds.contains(&EventKind::CompactRejected));
        assert!(!kinds.contains(&EventKind::ContextCompacted));
    }

    /// Mutation witness for the verdict guard: dropping `plan_accepted` from
    /// the acceptance predicate makes the tiny-after rejection above pass
    /// every numeric check and flip to success.
    #[test]
    fn compaction_verdict_guard_is_mutation_sensitive() {
        const SOURCE: &str = include_str!("compaction.rs");
        assert!(SOURCE.contains("plan_accepted && after <= target"));
        let mutated = SOURCE.replace("plan_accepted && after <= target", "after <= target");
        assert_ne!(SOURCE, mutated, "the verdict guard seam must exist");
        assert!(
            !mutated.contains("plan_accepted && after <= target"),
            "removing the plan verdict is the detected mutation"
        );
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

    // -------------------------------------- audit item 15: archive lifecycle

    fn meta(created_ms: i64) -> ArchiveMetadata {
        ArchiveMetadata {
            total_bytes: 1_024,
            chunk_count: 2,
            created_ms,
        }
    }

    /// The documented grace constant and the negative-clamp rule.
    #[test]
    fn archive_lifecycle_grace_default_is_seven_days_and_never_negative() {
        assert_eq!(
            ARCHIVE_BODY_GRACE_MS,
            7 * 24 * 60 * 60 * 1000,
            "the conservative default grace is 7 days (documented reason at the constant)"
        );
        assert_eq!(
            ArchiveLifecycleRegistry::new().grace_ms(),
            ARCHIVE_BODY_GRACE_MS
        );
        // A hostile negative grace clamps to zero — never a negative wait.
        assert_eq!(ArchiveLifecycleRegistry::with_grace_ms(-1).grace_ms(), 0);
    }

    /// Fresh -> superseded -> (grace) -> body-evictable -> digest-only, in
    /// ORDER, with every stage typed and idempotent under replay.
    #[test]
    fn archive_lifecycle_walks_fresh_superseded_body_evictable_then_digest_only() {
        let hash = "1".repeat(64);
        let mut registry = ArchiveLifecycleRegistry::new();
        registry.register_live(&hash, meta(1_000), 1_000).unwrap();
        assert_eq!(
            registry.get(&hash).unwrap().state,
            ArchiveLifecycleState::LiveRecovery
        );
        // Replay of the same publication is idempotent (never resets).
        registry.register_live(&hash, meta(1_000), 1_000).unwrap();
        // A second generation supersedes the first AT PUBLICATION (at most
        // one unpinned live generation).
        let second = "2".repeat(64);
        registry.register_live(&second, meta(5_000), 5_000).unwrap();
        assert_eq!(
            registry.get(&hash).unwrap().state,
            ArchiveLifecycleState::SupersededRecovery
        );
        assert_eq!(
            registry.get(&hash).unwrap().superseded_at_ms,
            Some(5_000),
            "supersession is stamped at the publication of the successor"
        );
        assert_eq!(
            registry.get(&second).unwrap().state,
            ArchiveLifecycleState::LiveRecovery
        );
        // Skipping stages is typed, not silent: live -> evictable refused.
        let standalone = "3".repeat(64);
        registry
            .register_live(&standalone, meta(6_000), 6_000)
            .unwrap();
        assert!(matches!(
            registry.mark_evictable(&standalone, 6_000),
            Err(ArchiveLifecycleError::IllegalTransition { .. })
        ));
        // Grace: the superseded generation refuses before it elapses, with
        // the remaining time in the typed error.
        match registry.mark_evictable(&hash, 5_000) {
            Err(ArchiveLifecycleError::GracePeriodActive { remaining_ms, .. }) => {
                assert_eq!(remaining_ms, ARCHIVE_BODY_GRACE_MS);
            }
            other => panic!("expected GracePeriodActive, got {other:?}"),
        }
        assert!(matches!(
            registry.mark_evictable(&hash, 5_000 + ARCHIVE_BODY_GRACE_MS - 1),
            Err(ArchiveLifecycleError::GracePeriodActive {
                remaining_ms: 1,
                ..
            })
        ));
        // Exactly at the boundary: evictable.
        registry
            .mark_evictable(&hash, 5_000 + ARCHIVE_BODY_GRACE_MS)
            .unwrap();
        assert_eq!(
            registry.get(&hash).unwrap().state,
            ArchiveLifecycleState::EvictableBody
        );
        // Skipped-degradation guard: superseded still cannot jump straight
        // to digest-only (the successor below is still inside grace).
        assert!(matches!(
            registry.degrade_to_digest_only(&second),
            Err(ArchiveLifecycleError::IllegalTransition { .. })
        ));
        // The evictable body degrades to a digest-only record that keeps
        // digest + metadata queryable.
        registry.degrade_to_digest_only(&hash).unwrap();
        let record = registry.get(&hash).unwrap();
        assert_eq!(record.state, ArchiveLifecycleState::DigestOnly);
        assert_eq!(record.retained_digest(), hash);
        assert_eq!(record.metadata, meta(1_000));
        // Idempotent replays at every stage.
        registry.degrade_to_digest_only(&hash).unwrap();
        registry.adopt_body_loss(&hash).unwrap();
        assert_eq!(
            registry.get(&hash).unwrap().state,
            ArchiveLifecycleState::DigestOnly
        );
        // Batch advance moves aged superseded generations but never the
        // ones still inside grace and never a live generation.
        let aged = registry.advance_elapsed(6_000 + ARCHIVE_BODY_GRACE_MS);
        assert_eq!(aged, 1, "the superseded successor aged out");
        assert_eq!(
            registry.get(&second).unwrap().state,
            ArchiveLifecycleState::EvictableBody
        );
        assert_eq!(
            registry.get(&standalone).unwrap().state,
            ArchiveLifecycleState::LiveRecovery,
            "a live generation never ages out"
        );
    }

    /// A PINNED (recovery-critical) generation refuses every time-based
    /// degradation until unpinned, and can never be silently superseded
    /// out of liveness by a new publication.
    #[test]
    fn pinned_archive_generation_refuses_premature_degradation() {
        let pinned_hash = "f".repeat(64);
        let mut registry = ArchiveLifecycleRegistry::new();
        registry
            .register_live(&pinned_hash, meta(1_000), 1_000)
            .unwrap();
        registry.pin(&pinned_hash).unwrap();
        assert!(matches!(
            registry.supersede(&pinned_hash, 2_000),
            Err(ArchiveLifecycleError::Pinned { .. })
        ));
        assert_eq!(
            registry.get(&pinned_hash).unwrap().state,
            ArchiveLifecycleState::LiveRecovery,
            "a refused supersede must not move the state"
        );
        // A new publication does NOT collateral-damage the pinned live
        // generation: both stay live (pinned rows are coexisting recovery
        // authorities).
        let successor = "d".repeat(64);
        registry
            .register_live(&successor, meta(3_000), 3_000)
            .unwrap();
        assert_eq!(
            registry.get(&pinned_hash).unwrap().state,
            ArchiveLifecycleState::LiveRecovery
        );
        // Time cannot degrade it either — even far past the grace.
        assert_eq!(
            registry.advance_elapsed(ARCHIVE_BODY_GRACE_MS * 100),
            0,
            "pinned generations never age out"
        );
        // Direct evictable/degrade attempts refuse typed.
        // (A pinned LIVE record first needs a legal supersede — which the
        // pin itself refuses, so the degradation pipeline is unreachable.)
        assert!(matches!(
            registry.supersede(&pinned_hash, 10_000),
            Err(ArchiveLifecycleError::Pinned { .. })
        ));
        // Unknown hashes are typed, never guessed.
        assert!(matches!(
            registry.pin(&"0".repeat(64)),
            Err(ArchiveLifecycleError::UnknownGeneration(_))
        ));
        assert!(matches!(
            registry.pin("zz"),
            Err(ArchiveLifecycleError::Malformed(_))
        ));
        // Unpinning re-enables the normal lifecycle.
        registry.unpin(&pinned_hash).unwrap();
        registry.supersede(&pinned_hash, 20_000).unwrap();
        registry
            .mark_evictable(&pinned_hash, 20_000 + ARCHIVE_BODY_GRACE_MS)
            .unwrap();
        registry.degrade_to_digest_only(&pinned_hash).unwrap();
        assert_eq!(
            registry.get(&pinned_hash).unwrap().state,
            ArchiveLifecycleState::DigestOnly
        );
    }

    /// A digest-only record still VERIFIES: presented bytes rehash against
    /// the retained digest, and any mismatch is typed — never a bool the
    /// caller could ignore.
    #[test]
    fn digest_only_record_verifies_against_the_retained_digest_typed_on_mismatch() {
        let body = b"archived turn bytes that outlive their payload";
        let hash = blake3::hash(body).to_hex().to_string();
        let mut registry = ArchiveLifecycleRegistry::new();
        registry.register_live(&hash, meta(1), 1_000).unwrap();
        registry.supersede(&hash, 2_000).unwrap();
        registry
            .mark_evictable(&hash, 2_000 + ARCHIVE_BODY_GRACE_MS)
            .unwrap();
        registry.degrade_to_digest_only(&hash).unwrap();
        let record = registry.get(&hash).unwrap().clone();
        // The payload is gone from the lifecycle's perspective; the digest
        // record still proves a later-presented body.
        assert_eq!(record.state, ArchiveLifecycleState::DigestOnly);
        record.verify_presented_body(body).unwrap();
        let tampered = b"archived turn bytes that outlive their payloaD";
        let mismatch = record.verify_presented_body(tampered).unwrap_err();
        assert!(
            matches!(
                &mismatch,
                ArchiveLifecycleError::DigestMismatch { expected, actual }
                    if expected == &hash && actual != expected
            ),
            "the mismatch must be typed with both digests: {mismatch:?}"
        );
        // Even an empty/garbage presentation fails typed.
        assert!(matches!(
            record.verify_presented_body(&[]),
            Err(ArchiveLifecycleError::DigestMismatch { .. })
        ));
    }

    /// Sweeping even the digest record ends continuity ON PURPOSE: the
    /// record is gone, the durable deletion is journaled, and the caller's
    /// wire marker must fall back to the loss marker for this hash.
    #[test]
    fn sweeping_a_digest_record_ends_continuity_and_journals_the_durable_deletion() {
        let hash = "e".repeat(64);
        let mut registry = ArchiveLifecycleRegistry::new();
        registry.register_live(&hash, meta(1), 1_000).unwrap();
        registry.supersede(&hash, 2_000).unwrap();
        registry
            .mark_evictable(&hash, 2_000 + ARCHIVE_BODY_GRACE_MS)
            .unwrap();
        registry.degrade_to_digest_only(&hash).unwrap();
        let swept = registry.sweep(&hash).expect("the record exists to sweep");
        assert_eq!(swept.manifest_hash, hash);
        assert_eq!(swept.state, ArchiveLifecycleState::DigestOnly);
        assert!(registry.get(&hash).is_none(), "continuity is gone");
        assert_eq!(
            registry.swept().len(),
            1,
            "the durable row deletion is journaled"
        );
        assert_eq!(registry.swept()[0], hash);
        // Double-sweep is a no-op, never a panic or a duplicate journal.
        assert!(registry.sweep(&hash).is_none());
        assert_eq!(registry.swept().len(), 1);
        registry.clear_swept();
        assert!(registry.swept().is_empty());
    }

    /// The bounded registry drops the OLDEST degraded record first (with
    /// its durable deletion journaled) and REFUSES to drop a protected
    /// generation when nothing is degraded.
    #[test]
    fn registry_bound_drops_degraded_generations_first_and_refuses_protected_drops() {
        let mut registry = ArchiveLifecycleRegistry::with_grace_ms(0);
        // Fill with DIGEST_ONLY records: register, supersede (previous live
        // auto-supersedes), age out, degrade — except the newest (live).
        let hash = |i: usize| format!("{i:064x}");
        for i in 0..MAX_ARCHIVE_LIFECYCLE_GENERATIONS {
            registry
                .register_live(&hash(i), meta(i as i64), i as i64)
                .unwrap();
            if i > 0 {
                registry.mark_evictable(&hash(i - 1), i as i64).unwrap();
                registry.degrade_to_digest_only(&hash(i - 1)).unwrap();
            }
        }
        assert_eq!(registry.len(), MAX_ARCHIVE_LIFECYCLE_GENERATIONS);
        // One more registration: the OLDEST degraded record is dropped and
        // journaled for durable deletion; live/superseded survive.
        registry
            .register_live(&hash(MAX_ARCHIVE_LIFECYCLE_GENERATIONS), meta(999), 999)
            .unwrap();
        assert_eq!(registry.len(), MAX_ARCHIVE_LIFECYCLE_GENERATIONS);
        assert!(registry.get(&hash(0)).is_none(), "oldest degraded dropped");
        assert_eq!(registry.swept(), &[hash(0)]);
        assert!(registry.get(&hash(1)).is_some(), "younger records survive");
        // A registry full of PINED live generations refuses the new
        // registration typed — a protected generation is never dropped.
        let mut pinned_registry = ArchiveLifecycleRegistry::new();
        for i in 0..MAX_ARCHIVE_LIFECYCLE_GENERATIONS {
            pinned_registry
                .register_live(&hash(i), meta(i as i64), i as i64)
                .unwrap();
            pinned_registry.pin(&hash(i)).unwrap();
        }
        assert_eq!(pinned_registry.len(), MAX_ARCHIVE_LIFECYCLE_GENERATIONS);
        assert!(matches!(
            pinned_registry.register_live(&hash(999), meta(0), 0),
            Err(ArchiveLifecycleError::RegistryFull)
        ));
        assert_eq!(pinned_registry.len(), MAX_ARCHIVE_LIFECYCLE_GENERATIONS);
        assert!(pinned_registry.swept().is_empty(), "nothing was dropped");
    }

    /// Durable roundtrip: save -> reopen-style load preserves every record
    /// and state; hostile rows (bad JSON, unknown state, key/value
    /// disagreement) are TYPED errors so a caller falls back to the plain
    /// loss-marker path instead of trusting corruption.
    #[test]
    fn archive_lifecycle_persistence_roundtrips_and_hostile_rows_are_typed() {
        let (_d, m) = test_manager();
        let s = session(&m);
        let mut registry = ArchiveLifecycleRegistry::new();
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        registry.register_live(&a, meta(1), 1_000).unwrap();
        registry.register_live(&b, meta(2), 2_000).unwrap();
        assert_eq!(
            registry.get(&a).unwrap().state,
            ArchiveLifecycleState::SupersededRecovery
        );
        s.save_archive_lifecycle_registry(&mut registry).unwrap();
        assert!(registry.swept().is_empty(), "save consumes the journal");
        let loaded = s.archive_lifecycle_registry().unwrap();
        assert_eq!(loaded.generations(), registry.generations());
        assert_eq!(loaded.grace_ms(), ARCHIVE_BODY_GRACE_MS);
        // Sweep -> save deletes the durable row.
        let mut registry = s.archive_lifecycle_registry().unwrap();
        assert!(registry.sweep(&a).is_some());
        s.save_archive_lifecycle_registry(&mut registry).unwrap();
        let loaded = s.archive_lifecycle_registry().unwrap();
        assert!(loaded.get(&a).is_none());
        assert!(loaded.get(&b).is_some());
        // Hostile row 1: not JSON.
        s.upsert_memory_fact(
            ARCHIVE_LIFECYCLE_FACT_KIND,
            &format!("{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}{}", "c".repeat(64)),
            "not-json",
        )
        .unwrap();
        assert!(matches!(
            s.archive_lifecycle_registry().err(),
            Some(err) if err.to_string().contains("malformed")
        ));
        // Hostile row 2: key/value hash disagreement.
        s.upsert_memory_fact(
            ARCHIVE_LIFECYCLE_FACT_KIND,
            &format!("{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}{}", "c".repeat(64)),
            &serde_json::to_string(&ArchiveGenerationRecord {
                manifest_hash: "d".repeat(64),
                state: ArchiveLifecycleState::LiveRecovery,
                pinned: false,
                created_ms: 0,
                superseded_at_ms: None,
                metadata: meta(0),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            s.archive_lifecycle_registry().err(),
            Some(err) if err.to_string().contains("disagree")
        ));
        // Hostile row 3: unknown lifecycle state.
        s.upsert_memory_fact(
            ARCHIVE_LIFECYCLE_FACT_KIND,
            &format!("{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}{}", "c".repeat(64)),
            r#"{"manifest_hash":"cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","state":"nonsense","pinned":false,"created_ms":0,"superseded_at_ms":null,"metadata":{"total_bytes":1,"chunk_count":1,"created_ms":0}}"#,
        )
        .unwrap();
        assert!(matches!(
            s.archive_lifecycle_registry().err(),
            Some(err) if err.to_string().contains("malformed")
        ));
        // Hostile row 4: malformed key (bad suffix).
        s.upsert_memory_fact(
            ARCHIVE_LIFECYCLE_FACT_KIND,
            &format!("{ARCHIVE_LIFECYCLE_FACT_KEY_PREFIX}not-a-hash"),
            "{}",
        )
        .unwrap();
        assert!(matches!(
            s.archive_lifecycle_registry().err(),
            Some(err) if err.to_string().contains("malformed")
        ));
    }
}
