//! `runtime::turn::parent_context`: the DURABLE delivery arm of the
//! internal-context channel.
//!
//! The op-keyed arm ([`AgentRuntime::store_internal_context`]) attaches an
//! already-rendered Faktor section to the operation it was submitted with
//! (`run_turn_input`/`run_turn_from_handoff`). This module is the second,
//! durable arm used by the ORCHESTRATOR's parent continuation: when a run's
//! child reaches its terminal state, the executor renders the child's
//! bounded typed [`ChildHandoff`] through the ONE section renderer and
//! records the section as a pending memory fact on the PARENT session
//! (kind [`PENDING_INTERNAL_CONTEXT_KIND`], key
//! `child-handoff/<child id>`).
//!
//! The parent's NEXT logical turn — whenever the daemon drives it — consumes
//! every pending section exactly once at drive start and folds it into the
//! volatile system slot, next to the op-keyed block
//! ([`TurnDriveState::new`]). The section is DATA rendered under the
//! Faktor-controlled `[faktor:internal-context]` header; it never becomes a
//! conversation message and never carries user authority.
//!
//! Durability + ack law: the fact is the durable delivery record (a crash
//! between the child's terminal persist and the parent's next turn loses
//! nothing), and consumption rewrites the value to the empty string — the
//! durable ack (legacy facts have no delete). An empty value is NEVER
//! rendered, so a consumed handoff cannot be re-delivered by a replay; a
//! re-record of the same child overwrites the row (latest state wins,
//! idempotent across re-attach).

/// Durable fact kind of the pending-internal-context delivery arm.
pub const PENDING_INTERNAL_CONTEXT_KIND: &str = "internal_context";

/// Key prefix of one child-handoff delivery inside that kind. The key is
/// `child-handoff/<child id>`; re-recording the same child overwrites.
pub const PENDING_INTERNAL_CONTEXT_KEY_PREFIX: &str = "child-handoff/";

/// Durable ack: rewrite the consumed entry to the empty value. Best-effort
/// by contract — a failed ack only means the section stays pending and the
/// parent's following turn re-receives it (at-least-once, never lost), so
/// the failure is logged, never fatal to the turn.
fn ack_internal_context_entry(handle: &faktor_session::SessionHandle, key: &str) {
    if let Err(error) = handle.upsert_memory_fact(PENDING_INTERNAL_CONTEXT_KIND, key, "") {
        tracing::warn!(
            session = %handle.id(),
            key,
            "pending internal-context ack failed: {error}"
        );
    }
}

/// Take the pending internal-context sections recorded for the session of
/// `handle`, in deterministic key order, acking each rendered entry once.
/// Empty when the session carries none; a fact read failure yields empty
/// and logs (the following drive re-reads the durable rows).
pub(crate) fn take_pending_internal_context(handle: &faktor_session::SessionHandle) -> String {
    // Bounded per-kind walk: the previous full fact-table scan got slower
    // with every turn's facts (O(n²) over a long task). 16 x 200 handoff
    // rows is far beyond the bounded child-control cap; exhausting the walk
    // is logged and leaves the untouched rows pending for the next drive.
    const PAGE: i64 = 200;
    const MAX_PAGES: usize = 16;
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut cursor: Option<(i64, String, String)> = None;
    for _ in 0..MAX_PAGES {
        let page = match handle.memory_facts_by_kind_page(
            PENDING_INTERNAL_CONTEXT_KIND,
            cursor.as_ref(),
            PAGE,
        ) {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(
                    session = %handle.id(),
                    "pending internal-context read failed: {error}"
                );
                return String::new();
            }
        };
        pending.extend(
            page.facts
                .into_iter()
                .filter(|(kind, key, value)| {
                    kind == PENDING_INTERNAL_CONTEXT_KIND
                        && key.starts_with(PENDING_INTERNAL_CONTEXT_KEY_PREFIX)
                        && !value.is_empty()
                })
                .map(|(_, key, value)| (key, value)),
        );
        match page.cursor {
            Some(next) if page.has_more => cursor = Some(next),
            _ => {
                cursor = None;
                break;
            }
        }
    }
    if cursor.is_some() {
        tracing::error!(
            session = %handle.id(),
            pages = MAX_PAGES,
            page_size = PAGE,
            "pending internal-context scan bound exhausted; older handoffs stay pending"
        );
    }
    pending.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (key, value) in pending {
        ack_internal_context_entry(handle, &key);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&value);
    }
    out
}
