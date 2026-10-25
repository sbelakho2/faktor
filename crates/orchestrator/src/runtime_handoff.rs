//! Durable child-handoff delivery and crash-window healing.
//!
//! Split from `runtime.rs` (recorded source-size ceiling) without changing
//! the delivery contract: the pending section and its watermark share ONE
//! store transaction so re-attach can heal a crash between the child's
//! terminal row and the handoff without ever resurrecting a section the
//! parent already consumed.
//!
//! The watermark is the child's durable lifecycle REVISION, never
//! `updated_ms`: two distinct terminal transitions (failed -> retried ->
//! done) can share one millisecond, and a wall-clock watermark then
//! suppresses the newer result. Revisions are per-child, monotonic and
//! persisted with the state they describe.

use faktor_agent::runtime::{
    InternalContextItem, ParentTurnInput, DEFAULT_CHILD_HANDOFF_TOKENS,
    PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX, PENDING_INTERNAL_CONTEXT_KEY_PREFIX,
    PENDING_INTERNAL_CONTEXT_KIND,
};

use super::*;

/// Durable encoding prefix of a revision watermark. New watermarks are
/// `rev:<generation>`; a bare integer is a LEGACY `updated_ms` watermark
/// written by an older daemon and is handled conservatively.
pub(crate) const HANDOFF_WATERMARK_PREFIX: &str = "rev:";

/// The durable watermark string of one delivered child generation.
pub(crate) fn handoff_watermark(revision: u64) -> String {
    format!("{HANDOFF_WATERMARK_PREFIX}{revision}")
}

/// One durable handoff delivery watermark, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveredWatermark {
    /// A post-migration revision watermark.
    Revision(u64),
    /// A pre-migration `updated_ms` watermark: it can only be trusted to
    /// cover the unmigrated generation 0.
    LegacyTimestamp,
    /// Hostile/corrupt durable bytes: fail closed (never re-deliver).
    Malformed,
}

impl DeliveredWatermark {
    fn parse(raw: &str) -> Self {
        match raw.strip_prefix(HANDOFF_WATERMARK_PREFIX) {
            Some(digits) => match digits.parse::<u64>() {
                Ok(revision) => Self::Revision(revision),
                Err(_) => Self::Malformed,
            },
            None => match raw.parse::<i64>() {
                Ok(_) => Self::LegacyTimestamp,
                Err(_) => Self::Malformed,
            },
        }
    }

    /// True when this delivery already covers `revision` (a no-op).
    ///
    /// A legacy timestamp covers only generation 0: a row that transitioned
    /// after the upgrade carries a newer revision, so its newer terminal
    /// result is delivered. A malformed value covers everything (fail
    /// closed — a duplicate is worse than a healing retry).
    fn covers(self, revision: u64) -> bool {
        match self {
            Self::Revision(delivered) => delivered >= revision,
            Self::LegacyTimestamp => revision == 0,
            Self::Malformed => true,
        }
    }

    fn is_malformed(self) -> bool {
        matches!(self, Self::Malformed)
    }
}

/// Bounded CAS retries when another re-attach wins the watermark race. A
/// re-read that already covers the target revision exits immediately, so
/// this bound only trips under pathological churn.
const MAX_WATERMARK_CAS_ATTEMPTS: usize = 4;

/// Durable fact keys of ONE child handoff delivery. The key is
/// RUN-QUALIFIED (`child-handoff/<run id>/<child id>`) because child ids are
/// minted per run (`child-0` in every plan): a parent session may own more
/// than one orchestration run, and a session-wide key would let one run's
/// terminal child overwrite (or watermark-suppress) another run's handoff —
/// a sibling-result leak. The run id is the same validated ASCII token the
/// durable registry/plan rows carry.
pub(crate) fn handoff_fact_keys(run_id: &str, child_id: &str) -> (String, String) {
    (
        format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{run_id}/{child_id}"),
        format!("{PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX}{run_id}/{child_id}"),
    )
}

impl OrchestratorRuntime {
    /// The parent continuation boundary (audit: typed child handoff
    /// consumption): hand ONE finished child's BOUNDED typed handoff to the
    /// PARENT session as durable internal context for its next logical turn
    /// — the child's transcript is NEVER inlined, never a user-role prompt.
    /// The section is rendered ONCE here by the channel's own renderer (the
    /// Faktor-controlled untrusted-DATA envelope) and recorded as a pending
    /// fact the runtime consumes exactly once at the parent's next drive.
    ///
    /// Delivery is generation-bound: the section and the `rev:<revision>`
    /// watermark land in ONE conditional store transaction. Equal or older
    /// already-delivered revisions are no-ops, a newer terminal generation
    /// always replaces the pending section, and concurrent re-attach calls
    /// race the CAS so only one write wins.
    ///
    /// Best-effort by contract — the run is already settled; an unavailable
    /// component (typed refusal from [`Self::child_handoff`]), a vanished
    /// parent session or a failed fact write is logged and skipped, and the
    /// durable child rows remain the retrievable source of truth. Crash
    /// window (healed): a crash between the child's terminal persist and this
    /// delivery leaves the pending section absent; [`Self::reattach`]
    /// re-derives the handoff from the durable terminal row
    /// ([`Self::heal_pending_child_handoffs`]), so the result is never lost.
    /// A section the parent already consumed (empty value) is never re-armed
    /// for the same generation.
    pub(super) fn deliver_child_handoff(
        &self,
        parent: SessionId,
        run_id: &str,
        child: &ChildRuntime,
    ) {
        // Cancellation is a terminal OUTCOME the parent must see: a cancelled
        // child is not a success and not a failure, but suppressing its
        // handoff made the parent context look as if the item simply never
        // reported (unavailable rendered as absent). The typed row already
        // carries `cancelled`, so the bounded handoff states it explicitly.
        if !matches!(
            child.state,
            ChildState::Done | ChildState::Failed | ChildState::Cancelled
        ) {
            return;
        }
        let child_session = match SessionId::try_from(child.session_id) {
            Ok(session) => session,
            Err(error) => {
                tracing::warn!(
                    child = %child.child_id,
                    "child handoff delivery skipped: session id {}: {}",
                    child.session_id,
                    error.message
                );
                return;
            }
        };
        let handle = match self.manager.get_session(parent) {
            Ok(Some(handle)) => handle,
            Ok(None) => {
                tracing::warn!(
                    session = parent.raw(),
                    "child handoff delivery: parent session gone"
                );
                return;
            }
            Err(error) => {
                tracing::warn!(
                    session = parent.raw(),
                    "child handoff delivery: parent session read failed: {error}"
                );
                return;
            }
        };
        let (key, watermark_key) = handoff_fact_keys(run_id, &child.child_id);
        // Check the durable generation BEFORE rendering: a consumed or
        // already-delivered section for this exact generation is a no-op.
        let mut expected = match handle
            .memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &watermark_key)
        {
            Ok(raw) => {
                if let Some(stamp) = raw.as_deref() {
                    let delivered = Self::parse_delivery_watermark(stamp);
                    if delivered.is_malformed() {
                        tracing::warn!(
                            session = parent.raw(),
                            watermark_key,
                            "child handoff watermark is malformed; refusing to re-deliver (no duplicate)"
                        );
                        return;
                    }
                    if delivered.covers(child.revision) {
                        return;
                    }
                }
                raw
            }
            Err(error) => {
                tracing::warn!(
                    session = parent.raw(),
                    watermark_key,
                    "child handoff watermark read failed (delivery skipped): {error}"
                );
                return;
            }
        };
        let handoff = match self.child_handoff(
            &child.child_id,
            child_session,
            DEFAULT_CHILD_HANDOFF_TOKENS,
        ) {
            Ok(handoff) => handoff,
            Err(error) => {
                tracing::warn!(
                    child = %child.child_id,
                    "child handoff delivery skipped (typed refusal): {error}"
                );
                return;
            }
        };
        let input = ParentTurnInput {
            user_message: None,
            internal: vec![InternalContextItem::ChildHandoff {
                provenance: "orchestrator:child-handoff".to_string(),
                handoff,
            }],
        };
        let section = match input.render_internal_bounded(DEFAULT_CHILD_HANDOFF_TOKENS) {
            Ok(Some(section)) => section,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(
                    child = %child.child_id,
                    "child handoff delivery skipped (render refused): {error}"
                );
                return;
            }
        };
        let watermark = handoff_watermark(child.revision);
        // Section + watermark are ONE store transaction via a CAS on the
        // exact previously-read watermark: a crash can never leave a
        // delivered section whose watermark is missing (ambiguous on
        // re-attach) or a watermark without its section, and two concurrent
        // re-attaches can never both land the same generation.
        for _ in 0..MAX_WATERMARK_CAS_ATTEMPTS {
            let facts = [
                (
                    PENDING_INTERNAL_CONTEXT_KIND,
                    key.as_str(),
                    section.as_str(),
                ),
                (
                    PENDING_INTERNAL_CONTEXT_KIND,
                    watermark_key.as_str(),
                    watermark.as_str(),
                ),
            ];
            match self.manager.store().upsert_memory_facts_if_unchanged(
                handle.id(),
                PENDING_INTERNAL_CONTEXT_KIND,
                &watermark_key,
                expected.as_deref(),
                &facts,
            ) {
                Ok(true) => return,
                Ok(false) => {
                    match handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &watermark_key) {
                        Ok(raw) => {
                            if let Some(stamp) = raw.as_deref() {
                                let delivered = Self::parse_delivery_watermark(stamp);
                                if delivered.is_malformed() {
                                    tracing::warn!(
                                    session = parent.raw(),
                                    watermark_key,
                                    "child handoff watermark became malformed; refusing to re-deliver (no duplicate)"
                                );
                                    return;
                                }
                                if delivered.covers(child.revision) {
                                    return;
                                }
                            }
                            expected = raw;
                        }
                        Err(error) => {
                            tracing::warn!(
                            session = parent.raw(),
                            watermark_key,
                            "child handoff watermark re-read failed (left for the next re-attach): {error}"
                        );
                            return;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        session = parent.raw(),
                        key,
                        "child handoff delivery write failed: {error}"
                    );
                    return;
                }
            }
        }
        tracing::warn!(
            session = parent.raw(),
            watermark_key,
            "child handoff delivery lost the watermark race repeatedly (left for the next re-attach)"
        );
    }

    /// Parse the durable child-handoff delivery watermark. New watermarks are
    /// `rev:<generation>`; a bare integer is a legacy `updated_ms` watermark.
    /// A malformed value is a typed absence that healing treats fail-closed
    /// (never a duplicate delivery).
    fn parse_delivery_watermark(raw: &str) -> DeliveredWatermark {
        DeliveredWatermark::parse(raw)
    }

    /// Re-attach healing of the crash window between a child's terminal
    /// durable row and its parent handoff. The REVISION watermark binds the
    /// section to the EXACT terminal generation, so:
    ///
    /// - a section (pending or consumed) whose watermark covers the row's
    ///   revision is never touched (a consumed ack is never resurrected);
    /// - a section from an OLDER generation (failed -> retried done, even in
    ///   the same millisecond) is refreshed, because the newer terminal
    ///   result was never delivered;
    /// - absence is filled.
    ///
    /// Legacy rows decode with revision 0: a missing watermark delivers once
    /// and records `rev:0`; a legacy timestamp watermark (or section without
    /// one) is trusted only for generation 0. A row that transitioned after
    /// the upgrade carries a newer revision and is therefore delivered.
    /// Malformed watermarks fail closed. Best effort: a vanished parent or a
    /// failed read is logged, never fatal to the re-attach (the run's own
    /// rows remain the authority).
    pub(super) fn heal_pending_child_handoffs(
        &self,
        parent: SessionId,
        run_id: &str,
        children: &HashMap<String, ChildRuntime>,
    ) {
        let Ok(Some(handle)) = self.manager.get_session(parent) else {
            tracing::warn!(
                session = parent.raw(),
                "child handoff healing skipped: parent session unavailable"
            );
            return;
        };
        let mut terminal: Vec<&ChildRuntime> = children
            .values()
            .filter(|child| {
                matches!(
                    child.state,
                    ChildState::Done | ChildState::Failed | ChildState::Cancelled
                )
            })
            .collect();
        terminal.sort_by(|a, b| a.child_id.cmp(&b.child_id));
        for child in terminal {
            let (key, watermark_key) = handoff_fact_keys(run_id, &child.child_id);
            let handoff = handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &key);
            let watermark = handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &watermark_key);
            match (handoff, watermark) {
                (Err(error), _) | (_, Err(error)) => tracing::warn!(
                    session = parent.raw(),
                    key,
                    "child handoff healing read failed (left for the next re-attach): {error}"
                ),
                // A section with a watermark: the delivery covers this
                // generation or an older one; only a NEWER generation is
                // refreshed.
                (Ok(Some(_)), Ok(Some(stamp))) => {
                    let delivered = Self::parse_delivery_watermark(&stamp);
                    if delivered.is_malformed() {
                        tracing::warn!(
                            session = parent.raw(),
                            watermark_key,
                            "child handoff watermark is malformed; refusing to re-deliver (no duplicate)"
                        );
                    } else if !delivered.covers(child.revision) {
                        self.deliver_child_handoff(parent, run_id, child);
                    }
                }
                // Legacy section without a watermark: conservatively treat
                // generation 0 as delivered (a consumed ack must never be
                // resurrected). A transition after the upgrade carries a
                // newer revision and must still reach the parent.
                (Ok(Some(_)), Ok(None)) => {
                    if child.revision > 0 {
                        self.deliver_child_handoff(parent, run_id, child);
                    }
                }
                // No section: a watermark covering this generation means the
                // delivery was consumed; an older one means the crash hit
                // before delivery (or a retry produced a newer generation).
                (Ok(None), Ok(Some(stamp))) => {
                    let delivered = Self::parse_delivery_watermark(&stamp);
                    if delivered.is_malformed() {
                        tracing::warn!(
                            session = parent.raw(),
                            watermark_key,
                            "child handoff watermark is malformed; refusing to re-deliver (no duplicate)"
                        );
                    } else if !delivered.covers(child.revision) {
                        self.deliver_child_handoff(parent, run_id, child);
                    }
                }
                // Upgrade window: a pre-run-qualified pending section or
                // watermark is still the ONCE delivery for this child. The
                // parent consumes that section as before; re-delivering
                // under the new key would duplicate it.
                (Ok(None), Ok(None)) => {
                    let legacy_key =
                        format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{}", child.child_id);
                    let legacy_watermark_key = format!(
                        "{PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX}{}",
                        child.child_id
                    );
                    match (
                        handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &legacy_key),
                        handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &legacy_watermark_key),
                    ) {
                        (Ok(None), Ok(None)) => {
                            self.deliver_child_handoff(parent, run_id, child)
                        }
                        (Err(error), _) | (_, Err(error)) => tracing::warn!(
                            session = parent.raw(),
                            "legacy child handoff read failed (left for the next re-attach): {error}"
                        ),
                        _ => {}
                    }
                }
            }
        }
    }
}
