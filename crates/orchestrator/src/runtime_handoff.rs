//! Durable child-handoff delivery and crash-window healing.
//!
//! Split from `runtime.rs` (recorded source-size ceiling) without changing
//! the contract: delivery is best-effort, but the pending section and its
//! watermark share ONE store transaction so re-attach can heal a crash
//! between the child's terminal row and the handoff without ever
//! resurrecting a section the parent already consumed.

use faktor_agent::runtime::{
    InternalContextItem, ParentTurnInput, DEFAULT_CHILD_HANDOFF_TOKENS,
    PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX, PENDING_INTERNAL_CONTEXT_KEY_PREFIX,
    PENDING_INTERNAL_CONTEXT_KIND,
};

use super::*;

impl OrchestratorRuntime {
    /// The parent continuation boundary (audit: typed child handoff
    /// consumption): hand ONE finished child's BOUNDED typed handoff to the
    /// PARENT session as durable internal context for its next logical turn
    /// — the child's transcript is NEVER inlined, never a user-role prompt.
    /// The section is rendered ONCE here by the channel's own renderer (the
    /// Faktor-controlled untrusted-DATA envelope) and recorded as a pending
    /// fact the runtime consumes exactly once at the parent's next drive.
    ///
    /// Best-effort by contract — the run is already settled; an unavailable
    /// component (typed refusal from [`Self::child_handoff`]), a vanished
    /// parent session or a failed fact write is logged and skipped, and the
    /// durable child rows remain the retrievable source of truth. Crash
    /// window (healed): a crash between the child's terminal persist and this
    /// delivery leaves the pending section absent; [`Self::reattach`]
    /// re-derives the handoff from the durable terminal row
    /// ([`Self::heal_pending_child_handoffs`]), so the result is never lost.
    /// A section the parent already consumed (empty value) is never re-armed.
    pub(super) fn deliver_child_handoff(&self, parent: SessionId, child: &ChildRuntime) {
        if !matches!(child.state, ChildState::Done | ChildState::Failed) {
            // A Cancelled child has no outcome to report to the parent.
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
        let key = format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{}", child.child_id);
        let watermark_key = format!(
            "{PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX}{}",
            child.child_id
        );
        let watermark = child.updated_ms.to_string();
        // Section + watermark are ONE store transaction: a crash can never
        // leave a delivered section whose watermark is missing (which would
        // make re-attach ambiguous) or a watermark without its section.
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
        if let Err(error) = self
            .manager
            .store()
            .upsert_memory_facts(handle.id(), &facts)
        {
            tracing::warn!(
                session = parent.raw(),
                key,
                "child handoff delivery write failed: {error}"
            );
        }
    }

    /// Parse the durable child-handoff delivery watermark (the child row's
    /// `updated_ms` at delivery). A malformed value is a typed absence that
    /// healing treats fail-closed (never a duplicate delivery).
    fn parse_delivery_watermark(raw: &str) -> Option<i64> {
        raw.parse::<i64>().ok()
    }

    /// Re-attach healing of the crash window between a child's terminal
    /// durable row and its parent handoff. The delivery watermark binds the
    /// section to the EXACT terminal state, so:
    ///
    /// - a section (pending or consumed) whose watermark covers the row is
    ///   never touched (a consumed ack is never resurrected);
    /// - a section from an OLDER state (failed -> retried done) is refreshed,
    ///   because the newer terminal result was never delivered;
    /// - absence is filled.
    ///
    /// Legacy sections without a watermark are treated as delivered
    /// (conservative: no duplicate) and malformed watermarks fail closed.
    /// Best effort: a vanished parent or a failed read is logged, never fatal
    /// to the re-attach (the run's own rows remain the authority).
    pub(super) fn heal_pending_child_handoffs(
        &self,
        parent: SessionId,
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
            .filter(|child| matches!(child.state, ChildState::Done | ChildState::Failed))
            .collect();
        terminal.sort_by(|a, b| a.child_id.cmp(&b.child_id));
        for child in terminal {
            let key = format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{}", child.child_id);
            let watermark_key = format!(
                "{PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX}{}",
                child.child_id
            );
            let handoff = handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &key);
            let watermark = handle.memory_fact_get(PENDING_INTERNAL_CONTEXT_KIND, &watermark_key);
            match (handoff, watermark) {
                (Err(error), _) | (_, Err(error)) => tracing::warn!(
                    session = parent.raw(),
                    key,
                    "child handoff healing read failed (left for the next re-attach): {error}"
                ),
                // A section with a watermark: same/older state is already
                // delivered; only a NEWER terminal state is refreshed.
                (Ok(Some(_)), Ok(Some(stamp))) => match Self::parse_delivery_watermark(&stamp) {
                    Some(ms) if ms >= child.updated_ms => {}
                    Some(_) => self.deliver_child_handoff(parent, child),
                    None => tracing::warn!(
                        session = parent.raw(),
                        watermark_key,
                        "child handoff watermark is malformed; refusing to re-deliver (no duplicate)"
                    ),
                },
                // Legacy delivery without a watermark: conservatively treat
                // it as delivered (a consumed ack must never be resurrected).
                (Ok(Some(_)), Ok(None)) => {}
                // No section: a watermark covering this state means the
                // delivery was consumed; an older/missing one means the
                // crash hit before delivery.
                (Ok(None), Ok(Some(stamp))) => match Self::parse_delivery_watermark(&stamp) {
                    Some(ms) if ms >= child.updated_ms => {}
                    Some(_) => self.deliver_child_handoff(parent, child),
                    None => tracing::warn!(
                        session = parent.raw(),
                        watermark_key,
                        "child handoff watermark is malformed; refusing to re-deliver (no duplicate)"
                    ),
                },
                (Ok(None), Ok(None)) => self.deliver_child_handoff(parent, child),
            }
        }
    }
}
