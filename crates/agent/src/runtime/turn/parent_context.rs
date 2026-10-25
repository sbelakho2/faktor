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
//! `child-handoff/<run id>/<child id>`).
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
//!
//! Exactly-once under CONCURRENT parent turns: the value is re-read with its
//! durable CAS stamp and claimed to the empty ack in ONE writer transaction.
//! Two consumers that observed the same section can never both render it:
//! the loser's CAS conflicts and skips the row (or, when a newer value won,
//! leaves it for the next turn). A failed ack still renders (at-least-once —
//! a store failure must never silently drop a child result).

/// Durable fact kind of the pending-internal-context delivery arm.
pub const PENDING_INTERNAL_CONTEXT_KIND: &str = "internal_context";

/// Key prefix of one child-handoff delivery inside that kind. The key is
/// `child-handoff/<run id>/<child id>` (child ids are per run, so the run
/// qualifier prevents two runs of one parent from overwriting each other);
/// re-recording the same run/child overwrites.
pub const PENDING_INTERNAL_CONTEXT_KEY_PREFIX: &str = "child-handoff/";

/// Companion WATERMARK of the last delivered child state inside that kind:
/// `child-handoff-delivered/<run id>/<child id>` = the child registry row's
/// `updated_ms`. It is written atomically WITH the section and survives the
/// parent's ack, so re-attach can distinguish "this exact terminal state was
/// delivered (pending or consumed)" from "a NEWER retry state was never
/// delivered" without ever re-arming a consumed handoff.
pub const PENDING_INTERNAL_CONTEXT_DELIVERED_PREFIX: &str = "child-handoff-delivered/";

/// Claim ONE observed pending section for this consumer and durably ack it.
/// `Some(section)` means THIS consumer exclusively owns the delivery (or a
/// store failure left it unacked and it must still be shown, never lost);
/// `None` means another consumer claimed it, a newer value replaced the one
/// observed, or the row vanished — in every case it must not be rendered
/// here.
fn claim_pending_internal_context(
    handle: &faktor_session::SessionHandle,
    key: &str,
    observed: &str,
) -> Option<String> {
    let (current, stamp) = match handle.memory_fact_get_stamped(PENDING_INTERNAL_CONTEXT_KIND, key)
    {
        Ok(Some(row)) => row,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                session = %handle.id(),
                key,
                "pending internal-context claim read failed; leaving it pending: {error}"
            );
            return None;
        }
    };
    if current.is_empty() || current != observed {
        // Consumed or superseded between the page read and the claim: this
        // consumer must not render the stale observation.
        return None;
    }
    match handle.upsert_memory_fact_cas(PENDING_INTERNAL_CONTEXT_KIND, key, "", Some(stamp)) {
        Ok(faktor_store::MemoryFactCas::Applied(_)) => Some(current),
        Ok(faktor_store::MemoryFactCas::Conflict | faktor_store::MemoryFactCas::Missing) => None,
        Err(error) => {
            // The ack failed but the section was genuinely observed: show it
            // (at-least-once) rather than lose the child's result.
            tracing::warn!(
                session = %handle.id(),
                key,
                "pending internal-context ack failed: {error}; delivering unacked"
            );
            Some(current)
        }
    }
}

/// Default per-turn token budget of the pending internal-context block.
/// The block rides the volatile system tail, so it must share the system
/// class budget instead of concatenating every pending child handoff at
/// once (200 finished children at the 2,048-token per-handoff bound would
/// otherwise produce a ~400k-token required section and fail the whole
/// turn). The unclaimed sections stay pending for later turns.
pub const DEFAULT_PENDING_INTERNAL_CONTEXT_TOKENS: usize = 4_096;

/// Drop ITEM lines already delivered by an earlier section in THIS claim.
/// The production delivery arm renders one child handoff per durable row, so
/// five siblings that report the same shared fact would otherwise repeat it
/// five times in the same parent turn. Non-item lines (section headers,
/// trust notice, item envelopes, unavailable markers) are preserved: the
/// envelope stays well-formed even when every body item was a duplicate.
fn dedupe_section_items(section: &str, seen: &mut std::collections::HashSet<String>) -> String {
    let mut out = String::with_capacity(section.len());
    for line in section.lines() {
        if let Some(item) = line.strip_prefix("- ") {
            let key = item.split_whitespace().collect::<Vec<_>>().join(" ");
            if key.len() >= 8 && !seen.insert(key) {
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Take the pending internal-context sections recorded for the session of
/// `handle`, in deterministic key order, claiming each rendered entry once.
/// Empty when the session carries none; a fact read failure yields empty
/// and logs (the following drive re-reads the durable rows). Concurrent
/// consumers partition the pending set: every section is rendered by
/// exactly one of them (see [`claim_pending_internal_context`]).
#[cfg(test)]
pub(crate) fn take_pending_internal_context(handle: &faktor_session::SessionHandle) -> String {
    take_pending_internal_context_bounded(handle, DEFAULT_PENDING_INTERNAL_CONTEXT_TOKENS)
}

/// [`take_pending_internal_context`] with an explicit token budget. At most
/// that many estimated tokens are claimed and returned this turn (the first
/// section is always claimed so an oversized individual handoff cannot
/// starve the queue); everything not claimed stays pending and is served by
/// the next drive.
pub(crate) fn take_pending_internal_context_bounded(
    handle: &faktor_session::SessionHandle,
    budget_tokens: usize,
) -> String {
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
    let mut used = 0usize;
    let mut seen_items: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (key, value) in pending {
        let tokens = faktor_context::Estimator.estimate_tokens(&value);
        if !out.is_empty() && used.saturating_add(tokens) > budget_tokens {
            // Budget exhausted: leave the remaining durable rows pending
            // for the next drive (never claim a section we cannot render).
            continue;
        }
        let Some(claimed) = claim_pending_internal_context(handle, &key, &value) else {
            continue;
        };
        let claimed = dedupe_section_items(&claimed, &mut seen_items);
        used = used.saturating_add(faktor_context::Estimator.estimate_tokens(&claimed));
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&claimed);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};

    fn parent_session() -> (tempfile::TempDir, faktor_session::SessionHandle) {
        let dir = tempfile::tempdir().unwrap();
        let manager = faktor_session::SessionManager::open(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
        )
        .unwrap();
        let ws = manager.create_workspace("/root").unwrap();
        let session = manager.create_session(ws, "parent", "fake", "m").unwrap();
        (dir, session)
    }

    /// New-angle handoff probe: two parent turns racing at the durable
    /// delivery arm must partition the pending sections — every child
    /// handoff is rendered exactly once across the consumers, never twice
    /// and never lost. (The historical page-read + unconditional ack let
    /// both consumers read the same value before either acked.)
    #[test]
    fn fault_probe_concurrent_handoff_consumers_deliver_exactly_once() {
        let (_dir, handle) = parent_session();
        let total = 64usize;
        for i in 0..total {
            handle
                .upsert_memory_fact(
                    PENDING_INTERNAL_CONTEXT_KIND,
                    &format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{i:03}"),
                    &format!("[[SECTION-{i:03}]]"),
                )
                .unwrap();
        }
        let threads = 4usize;
        let barrier = Arc::new(Barrier::new(threads));
        let outputs = std::thread::scope(|scope| {
            let mut join = Vec::new();
            for _ in 0..threads {
                let handle = handle.clone();
                let barrier = barrier.clone();
                join.push(scope.spawn(move || {
                    barrier.wait();
                    take_pending_internal_context(&handle)
                }));
            }
            join.into_iter()
                .map(|j| j.join().expect("consumer thread"))
                .collect::<Vec<_>>()
        });
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for output in outputs {
            for line in output.lines().filter(|line| !line.is_empty()) {
                *counts.entry(line.to_string()).or_insert(0) += 1;
            }
        }
        assert_eq!(
            counts.len(),
            total,
            "every handoff was delivered: {counts:?}"
        );
        for i in 0..total {
            let key = format!("[[SECTION-{i:03}]]");
            assert_eq!(
                counts.get(&key),
                Some(&1),
                "handoff {key} must be delivered exactly once: {counts:?}"
            );
        }
        // Every claimed section is durably acked; a later turn sees nothing.
        for i in 0..total {
            let value = handle
                .memory_fact_get(
                    PENDING_INTERNAL_CONTEXT_KIND,
                    &format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{i:03}"),
                )
                .unwrap();
            assert_eq!(value.as_deref(), Some(""), "row {i} is acked");
        }
        assert!(take_pending_internal_context(&handle).is_empty());
    }

    /// 200 finished children must never concatenate into one over-budget
    /// required section: each drive claims only its token share and the
    /// rest stay pending, so all 200 are delivered exactly once over bounded
    /// turns (the old code claimed all 200 in one call).
    /// Sibling fact dedupe at the PRODUCTION delivery arm: each child
    /// handoff is a separate durable row, so the dedupe must happen when the
    /// parent claims the batch — not only when a caller renders items
    /// together. A shared fact appears once; unique per-child facts survive.
    #[test]
    fn pending_sections_dedupe_shared_sibling_facts() {
        let (_dir, handle) = parent_session();
        for i in 0..5 {
            let section = format!(
                "[faktor:internal-context]\n[internal-context-item kind=child-handoff provenance=orchestrator:child-handoff]\n[child-handoff]\n- shared fact SHARED-SIBLING-FACT\n- unique fact UNIQUE-{i}\n[internal-context-item-end]\n[/faktor:internal-context]\n"
            );
            handle
                .upsert_memory_fact(
                    PENDING_INTERNAL_CONTEXT_KIND,
                    &format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{i}"),
                    &section,
                )
                .unwrap();
        }
        let out = take_pending_internal_context_bounded(&handle, 4_096);
        assert_eq!(
            out.matches("SHARED-SIBLING-FACT").count(),
            1,
            "the shared sibling fact is delivered once: {out}"
        );
        for i in 0..5 {
            assert!(
                out.contains(&format!("UNIQUE-{i}")),
                "unique child {i} fact survives: {out}"
            );
        }
        assert!(take_pending_internal_context(&handle).is_empty());
    }

    #[test]
    fn two_hundred_pending_handoffs_are_delivered_in_bounded_turns() {
        let (_dir, handle) = parent_session();
        let total = 200usize;
        for i in 0..total {
            handle
                .upsert_memory_fact(
                    PENDING_INTERNAL_CONTEXT_KIND,
                    &format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}{i:03}"),
                    &format!("SECTION-{i:03} {}", "x".repeat(600)),
                )
                .unwrap();
        }
        let mut delivered = 0usize;
        let mut turns = 0usize;
        let started = std::time::Instant::now();
        loop {
            let out = take_pending_internal_context_bounded(&handle, 512);
            if out.is_empty() {
                break;
            }
            let tokens = faktor_context::Estimator.estimate_tokens(&out);
            assert!(
                tokens <= 512,
                "one drive's pending block must fit its budget, got {tokens}"
            );
            delivered += out.matches("SECTION-").count();
            turns += 1;
            assert!(turns <= total + 1, "the queue must drain");
        }
        eprintln!(
            "HANDOFF200 delivered={delivered} turns={turns} elapsed_ms={}",
            started.elapsed().as_millis()
        );
        assert_eq!(delivered, total, "every handoff delivered exactly once");
        assert!(turns > 1, "200 handoffs must span bounded turns");
        assert!(take_pending_internal_context(&handle).is_empty());
    }

    /// New-angle handoff probe: a consumer that observed a section which was
    /// superseded before its claim must not render the stale value (the
    /// newer child state belongs to the next turn).
    #[test]
    fn fault_probe_superseded_handoff_is_never_rendered_stale() {
        let (_dir, handle) = parent_session();
        let key = format!("{PENDING_INTERNAL_CONTEXT_KEY_PREFIX}child-7");
        handle
            .upsert_memory_fact(PENDING_INTERNAL_CONTEXT_KIND, &key, "OLD")
            .unwrap();
        // Simulate another turn claiming/re-recording between page read and
        // claim: the helper compares the CURRENT row to the observed value.
        handle
            .upsert_memory_fact(PENDING_INTERNAL_CONTEXT_KIND, &key, "NEW")
            .unwrap();
        assert_eq!(
            claim_pending_internal_context(&handle, &key, "OLD"),
            None,
            "a superseded observation is never rendered"
        );
        assert_eq!(
            claim_pending_internal_context(&handle, &key, "NEW").as_deref(),
            Some("NEW")
        );
        assert_eq!(claim_pending_internal_context(&handle, &key, "NEW"), None);
    }
}
