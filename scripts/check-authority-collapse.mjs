#!/usr/bin/env node
// Authority-collapse static check (closure plan item: "a mechanical
// authority-collapse checker with explicit allowlist").
//
// The six laws say: an UNREADABLE authority read (store failure, decode
// error, I/O error) may never silently become absence/default/ALL. This
// checker rejects the error→absence TRANSFORMATION PATTERNS in ALL
// production Rust:
//
//   1. a `Result<...>` consumed as `.ok()` / `.ok().flatten()` /
//      `.unwrap_or_default()` / `.unwrap_or(...)` / `.unwrap_or_else(...)` /
//      `.map_or(...)` on an authority-flavored receiver (handle/store/plane/
//      session reads, memory facts, task rows, ledgers, checkpoints, tokens,
//      workers, leases, the instruction resolver);
//   2. a `let Ok(...)` / `if let Ok(...)` binding on an authority read — a
//      value pattern on a `Result`: the reviewer must confirm the else /
//      fallthrough branch refuses typed (or is a documented absence), so
//      EVERY such site is a reviewed allowlist entry, never auto-passed;
//   3. an `Err(_)` arm whose body fabricates a default — `None`, `{}`,
//      `vec![]`, `Vec::new()`, `String::new()`, `false`, `0`, or a
//      `return <default>` shape.
//
// DISCOVERY IS MECHANICAL AND TOTAL — there is no manual file array and no
// annotation gate: every production Rust file under `crates/*/src/**` is
// scanned. (The former `@authority-collapse: authority-bearing` module
// annotations are OBSOLETE and grant nothing; they may remain as historical
// markers, but a hit is a hit with or without one.) Test-only code is
// excluded by construction:
//
//   * inline `#[cfg(test)]`-gated items are stripped with brace-matched
//     ranges on the same comment/string mask every pattern runs on;
//   * out-of-line modules declared ONLY as test-gated (`#[cfg(test)] mod
//     foo;`, `#[cfg(all(test, unix))] #[path = "$file"] mod foo;`) are
//     resolved from their declaring file and excluded — a file with ANY
//     non-test-gated declaration stays production code.
//
// The 2026 child-handoff defect (crates/orchestrator/src/merge.rs, a
// production file that carried NO annotation) was invisible to the previous
// annotation-gated checker; that class can never recur: a NEW module is
// discovered by the tree walk alone, and the --selftest fixtures prove a
// planted child-handoff/verification authority using error→absence is
// flagged WITHOUT adding any filename or annotation first.
//
// Every remaining hit must be listed in ALLOWLIST with a reason AND an
// owner; a stale allowlist entry (no longer matching, or matching a
// different number of times) is itself a failure so reviews stay honest.
// Durable-authority reads are NOT exempted: an entry is only legitimate
// where absence is the documented semantics (optional lookups, display-only
// projections, conservative fail-closed handling that REFUSES typed or
// reports a violation rather than proceeding).
//
// Exit codes: 0 pass; 1 violations/stale allowlist; 2 usage.
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// Reviewed exceptions. `line_contains` is a literal substring of the source
// line; `count` must match exactly (changes force re-review); `owner` names
// the authority responsible for the reviewed semantics. An entry may only
// cover a documented absence semantics / typed-refusal shape — NEVER a
// durable-authority read whose error collapses silently.
const ALLOWLIST = [

  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(Some(handle)) = manager.get_session(session_id) else {',
    count: 1,
    reason:
      'derive_child_projection (public registry projection): documented derive-only contract — the registry JSON is deliberately never rewritten, and an unreadable/missing session row leaves the stored value untouched (never a fabricated default/zero); durable readers reject malformed registry rows typed at the decode boundary and every registry read re-derives. The projection never gates completion, budgets or writes.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(ds) = handle.orchestrator_drive_state_get() {',
    count: 1,
    reason:
      'derive_child_projection execution-phase leg: derive-only projection (same reviewed contract); an unavailable drive-state row cannot override the stored phase, and the phase is presentational/re-derived on every registry read. It is never a lifecycle authority.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(task_id) = handle.task_id() {',
    count: 1,
    reason:
      'derive_child_projection budget leg (id read): derive-only projection; an unreadable task id derives nothing, leaving the previously derived value — never a fabricated cap and never an uncapped claim from a failed read of the ONE budget authority.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(Some(task)) = handle.get_task(task_id) {',
    count: 1,
    reason:
      'derive_child_projection budget leg (task row): derive-only projection; an unreadable task row leaves the stored derived value instead of zeroing or inflating the mirror; the child session\'s own durable budget governs enforcement.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Some(session) = manager.get_session(session_id).ok().flatten() else {',
    count: 1,
    reason:
      'registry_violations fail-loud audit: an unreadable or absent child session is REPORTED as a violation (never silently skipped), so the error cannot masquerade as consistency.',
    owner: 'orchestrator registry audit',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(srow) = session.row() else {',
    count: 1,
    reason:
      'registry_violations fail-loud audit: an unreadable session row is reported as a typed violation naming the child; the audit never treats it as agreement.',
    owner: 'orchestrator registry audit',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(Some(identity)) = session.orchestrator_child_identity_get() else {',
    count: 1,
    reason:
      'registry_violations fail-loud audit: an unreadable/absent identity row is reported as a violation of the zero-orphan identity contract.',
    owner: 'orchestrator registry audit',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(handles) = manager.list_sessions(None) {',
    count: 1,
    reason:
      'registry_violations reverse-direction completeness scan: additive only — the forward direction above already reported every unreadable registry row as a violation; a failed enumeration cannot certify consistency it did not read, and the caller refuses typed whenever the violation list is non-empty.',
    owner: 'orchestrator registry audit',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(Some(record)) = session.active_turn_record() {',
    count: 3,
    reason:
      'crash-recovery turn-record handling: absence of an active record is the normal state; an unreadable record is left for the next recovery pass (the subsequent `session.state()?` read is typed, and recovery refuses typed on unreadable state). The cancel/abort leg is bounded and best-effort by documented contract: the control row was already durably enqueued and acked above.',
    owner: 'orchestrator child recovery',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(Some(session)) = self.manager.get_session(child_session_id) else {',
    count: 1,
    reason:
      'recover_run redrive scan: a child whose session cannot be read is not driven blind; registry_violations (the zero-orphan audit) reports unreadable child sessions as violations, and the retry admission below refuses typed when it cannot read the durable control rows.',
    owner: 'orchestrator child recovery',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(pending) = session.orchestrator_ctl_pending() else {',
    count: 1,
    reason:
      'recover_run control-pending read: an unreadable control plane means the scan cannot decide to drive (conservative: no blind redrive); the durable control rows remain for the next recovery pass, and the ack/admission paths refuse typed.',
    owner: 'orchestrator child recovery',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'child_task_budget_cap mirror derivation (task row leg): an unreadable task row yields no mirror value; the durable child-side budget remains the enforcement authority and registry reads re-derive.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'if let Ok(Some(record)) = handle.active_turn_record() {',
    count: 1,
    reason:
      'child drive fresh-turn fallback: no readable active record means no resumable turn for this handle; the agent drive re-reads durable state itself and its crash-recovery resolves unfinished tool runs typed, so an unreadable record cannot silently complete an unfinished turn.',
    owner: 'orchestrator child recovery',
  },
  {
    file: 'crates/orchestrator/src/runtime.rs',
    line_contains: 'let Ok(Some(session)) = self.manager.get_session(session_id) else {',
    count: 3,
    reason:
      'note_child_phase / record_blocker_audit / record_blocker_resolved_audit: DOCUMENTED best-effort by contract — "the phase is a projection, never an authoritative lifecycle transition, so a failure is logged and the in-memory projection still carries the intended phase" and "failures are logged, never fatal (the durable row already carries the truth)".',
    owner: 'orchestrator child audit',
  },
  {
    file: 'crates/agent/src/runtime/compaction.rs',
    line_contains: 'let Ok(row) = handle.row() else {',
    count: 1,
    reason:
      'semantic consult on an unreadable session row escalates to SemanticTurnState::unknown (fail-closed), never absence — the else branch is the conservative Unknown.',
    owner: 'agent semantic consult authority',
  },
  {
    file: 'crates/agent/src/runtime/compaction.rs',
    line_contains: '.ok()',
    count: 2,
    reason:
      'workspace-root resolve inside the consult, plus the optional per-session compaction-model fact and the task-scope hint for the evidence archive: a failed resolve degrades to the explicit revision-unavailable branch (Unknown with a warn), an absent model fact means inherit, and a missing task id scopes the archive to the session/workspace — none fabricates a snapshot identity, model or task.',
    owner: 'agent semantic consult authority',
  },
  {
    file: 'crates/agent/src/runtime/media.rs',
    line_contains: 'let model = handle.model().unwrap_or_default();',
    count: 1,
    reason:
      'the model string is consumed only to name the refusing model in the media-gate ERROR message; the gate itself (caps.vision/document_capable) decides before this read, so an unreadable model name degrades a diagnostic string, never a delivery decision.',
    owner: 'agent media admission authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: 'let Ok(identity) = handle.identity() else {',
    count: 1,
    reason:
      'evidence-poll archive on an unreadable identity: the else branch logs a typed warn and SKIPS the archive; no guessed workspace id, no fabricated identity.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'session workspace-root resolution: a failed resolution is converted into a TYPED error by the immediately following ok_or_else — the failure is surfaced, never silently absent.',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: 'spent_tokens: handle.spent_tokens().unwrap_or(task.budget.spent_tokens),',
    count: 1,
    reason:
      'settlement budget view: an unreadable live counter falls back to the DURABLE task-row budget value (itself an authority), never to zero or a fabricated number.',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: 'let spent_tokens = handle.spent_tokens().unwrap_or(task.budget.spent_tokens);',
    count: 1,
    reason:
      'task-budget restoration: an unreadable live counter falls back to the DURABLE task row value, never to zero (same reviewed semantics as the settlement view above).',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/shell_changes.rs',
    line_contains: 'let Ok(listing) = root.list_entries(&dir_rel, SHELL_AUTHORITY_DIR_ENTRIES) else {',
    count: 1,
    reason:
      'shell-change walk: an unlistable authority directory skips THAT directory only; the certification path separately refuses when its own evidence is unreadable (the conservative direction).',
    owner: 'agent shell-change authority',
  },
  {
    file: 'crates/agent/src/runtime/shell_changes.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'manifest entry normalization: an unparseable entry yields None so the entry is treated as NOT verified; absence of proof can only tighten the verdict, never certify.',
    owner: 'agent shell-change authority',
  },
  {
    file: 'crates/agent/src/runtime/shell_changes.rs',
    line_contains: 'let Ok(rows) = handle.tool_runs_with_pre_manifest(64) else {',
    count: 1,
    reason:
      'an unreadable tool-run scan returns false ("no manifest-backed runs observed") which REFUSES the change-set claim, never fabricates one.',
    owner: 'agent shell-change authority',
  },
  {
    file: 'crates/agent/src/runtime/tool_loop.rs',
    line_contains: 'handle.task_id().unwrap_or_else(|_| TaskId::new(1)),',
    count: 1,
    reason:
      'tool-output ARCHIVE key for a bounded artifact: the archive key falls back to TaskId(1) so archiving never panics; the archive is a copy of already-produced output, never an authority (audit provenance: Small outputs keep today\'s byte-identical body).',
    owner: 'agent tool loop',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive.rs',
    line_contains: 'let token = handle.turn_cancellation(admitted.op_id).unwrap_or_default();',
    count: 1,
    reason:
      'cancellation-parent lookup for an ADMITTED operation: absence of a parent token is a legal root state (not a store failure).',
    owner: 'agent turn drive',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive.rs',
    line_contains: 'Err(_) => Vec::new(),',
    count: 1,
    reason:
      'outcome-ledger samples fold: a store error yields no durable samples; the caller KEEPS the in-memory facts.samples fallback (the arm above) and only the advisory outcome samples degrade.',
    owner: 'agent turn drive',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive_phases.rs',
    line_contains: '.unwrap_or_else(|| st.handle.title().unwrap_or_default());',
    count: 1,
    reason:
      'evidence-query prompt text: display-only context derived from the newest user message; the session title is the documented fallback, never an authority input.',
    owner: 'agent turn planning',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let mut tasks = handle.list_tasks().unwrap_or_default();',
    count: 1,
    reason:
      'review-context support read: an unreadable durable task list yields no durable criteria; the caller still carries criteria_rows and the goal text, so the reviewer never receives a silently empty package when a goal exists (this helper never gates completion).',
    owner: 'agent turn state',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let preferred = handle.task_id().ok();',
    count: 1,
    reason:
      'advisory task preference for the review package: an unreadable task id falls back to the session\'s first task; the value never gates completion, rebinds a task or feeds a proof.',
    owner: 'agent turn state',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: '.unwrap_or_default()',
    count: 2,
    reason:
      'Two reviewed Option projections: (a) after a positive position selection on a non-empty task list, an unreachable default kept for the total function shape; (b) the task-create template goal, used only when the preflight proved no task row exists, so an unreadable projection yields the documented empty goal instead of fabricating a task identity (the existing-row path patches under a revision CAS and never takes this arm).',
    owner: 'agent turn state',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let last = handle.last_event_seq().ok()??;',
    count: 1,
    reason:
      'last_event_kind diagnostic: an unreadable event sequence is explicit absence (None), never a fabricated event, and the value only classifies a display/telemetry decision.',
    owner: 'agent turn state',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: '.and_then(|h| cas.get_bounded(h, REVIEW_SIDE_BOUND).ok().flatten());',
    count: 1,
    reason:
      'review after-side fallback chain: a CAS read failure falls through to the workspace read, which REFUSES typed on failure — no silent absence can result.',
    owner: 'agent verification attribution',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: 'Err(_) => Ok(None),',
    count: 1,
    reason:
      'fingerprint observation: a hashing failure yields NO observation (the caller reports unavailable separately); it never fabricates a content identity.',
    owner: 'agent verification attribution',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: '.unwrap_or_default()',
    count: 2,
    reason:
      'Two Option projections AFTER typed reads: (a) the proof-basis criteria fold over `handle.get_task(task_id)?` — a missing task row is legal absence (empty criteria) and the read error already propagated with `?`; (b) `row.map(|t| t.acceptance_criteria)` after `self.session_task(handle)?` — the option only distinguishes a not-yet-seeded row from a seeded one, and a store failure is a typed refusal above.',
    owner: 'agent verification attribution',
  },
  {
    file: 'crates/browser/src/network.rs',
    line_contains: 'let response = params.get("response").cloned().unwrap_or(Value::Null);',
    count: 1,
    reason:
      'CDP event FIELD access (Option::unwrap_or on a JSON object field), not a Result read: a responseReceived event without a response object yields a null field; status/mime parsing below stays typed.',
    owner: 'browser CDP ingestion',
  },
  {
    file: 'crates/cli/src/daemon/builder.rs',
    line_contains: '.map(|row| (row.id.raw(), store.ledger_max_seq(row.id).unwrap_or(-1)))',
    count: 1,
    reason:
      'daemon STARTUP RECONSTRUCTION stamp (audit item): -1 is the explicit "no ledger rows" sentinel of the stamp and only orders recovery work; the ledger read itself is verified above, so a store failure cannot silently reorder recovery into skipping it.',
    owner: 'cli daemon builder',
  },
  {
    file: 'crates/cli/src/main.rs',
    line_contains: 'let ws = match session.create_workspace(workspace.to_str().unwrap_or(".")) {',
    count: 1,
    reason:
      'window-matched Option: `workspace.to_str().unwrap_or(".")` is an OS-string conversion for the CLI workspace default; the create_workspace call itself is matched by `match` and its Err arm refuses loudly below.',
    owner: 'cli main',
  },
  {
    file: 'crates/cli/src/tools_market.rs',
    line_contains: 'Err(_) => false,',
    count: 1,
    reason:
      'egress destination classifier: an unparseable target is NOT allowed (false = deny); absence of a match can only tighten the decision.',
    owner: 'cli tools market',
  },
  {
    file: 'crates/cli/src/tools_market.rs',
    line_contains: 'store.browser_profile_states(&id).ok().and_then(|rows| {',
    count: 1,
    reason:
      'browser profile verification: a failed state read yields no matching row, which renders as "unverified" in the report — never as a verified profile.',
    owner: 'cli tools market',
  },
  {
    file: 'crates/commerce/src/result.rs',
    line_contains: 'Err(_) => false,',
    count: 1,
    reason:
      'compact-result size probe: an unparseable JSON result is not "compact-target-sized"; false only routes the result down the conservative (non-compact) path.',
    owner: 'commerce connectors',
  },
  {
    file: 'crates/commerce/src/service.rs',
    line_contains: 'Err(_) => Vec::new(),',
    count: 2,
    reason:
      'poisoned registry mutex yields an empty HEALTH SNAPSHOT (advisory surface); the connector registry itself is in-memory and rebuilt on demand, no durable row is concealed.',
    owner: 'commerce connectors',
  },
  {
    file: 'crates/evidence/src/store.rs',
    line_contains: 'let cas = Cas::open(root.clone()).unwrap_or_else(|_| Cas::new(root));',
    count: 1,
    reason:
      'evidence CAS handle construction: falling back to a fresh handle over the SAME root; every blob operation below still verifies by digest and refuses on mismatch, so no evidence is fabricated or lost by the fallback.',
    owner: 'evidence store',
  },
  {
    file: 'crates/fs/src/workspace_service.rs',
    line_contains: 'u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)',
    count: 1,
    reason:
      'monotonic clock conversion: u64::try_from failing is unreachable for elapsed millis < u64::MAX; the saturating fallback is the conservative direction (worker considered overdue), never a zero.',
    owner: 'fs workspace service',
  },
  {
    file: 'crates/git/src/lib.rs',
    line_contains: 'owner_session: wt.owner_session.map(|s| s.raw()).unwrap_or(0),',
    count: 1,
    reason:
      'window-matched Option: `owner_session` is an Option<SessionId> on the worktree metadata; 0 is the documented "no owning session" value of this projection.',
    owner: 'git worktree metadata',
  },
  {
    file: 'crates/index/src/cold.rs',
    line_contains: '.ok()?;',
    count: 1,
    reason:
      'cold-tier generation load: an unopenable/unlistable generation directory yields None, which the caller reports as "no stale generation" and refuses to trust a truncated set — the conservative direction.',
    owner: 'index cold tier',
  },
  {
    file: 'crates/index/src/service/mod.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'index state read: an unreadable state row yields no state (the caller treats None as "no published index" and rebuild refusal paths apply); never a fabricated generation.',
    owner: 'index service',
  },
  {
    file: 'crates/ollama/src/lib.rs',
    line_contains: '.unwrap_or_else(panicked)',
    count: 1,
    reason:
      'panic payload conversion in the blocking-call bridge: `panicked` formats a JoinError into a typed internal error; not a read collapse.',
    owner: 'ollama adapter',
  },
  {
    file: 'crates/cli/src/daemon/serve.rs',
    line_contains: '.unwrap_or_default()',
    count: 1,
    reason:
      'ACP administrative session listing (list_sessions): an unreadable durable list renders as an empty admin view rather than failing the ACP request; every session OPERATION still resolves its handle (and refuses typed) through the durable read, so no authority is derived from the empty listing.',
    owner: 'cli acp surface',
  },
  {
    file: 'crates/agent/src/runtime/turn/queue.rs',
    line_contains: 'let Ok(Some(handle)) = self.deps.session.get_session(session) else {',
    count: 1,
    reason:
      'queue post-exhaustion error AUDIT path: the runner has already logged the typed failure and left the durable rows pending for the next drain; a vanished session ends the diagnostic, it cannot fake progress or absence of the durable rows.',
    owner: 'agent queue runner',
  },
  {
    file: 'crates/orchestrator/src/caps.rs',
    line_contains: 'if let Ok(Some(mut identity)) = session.orchestrator_child_identity_get() {',
    count: 1,
    reason:
      'capability projection refresh: only an EXISTING identity row is updated; an unreadable/absent row is a no-op here, and the spawn path that REQUIRES an identity refuses typed at its own boundary.',
    owner: 'orchestrator caps projection',
  },
  {
    file: 'crates/orchestrator/src/completion_steps.rs',
    line_contains: '.unwrap_or_else(|| op.handle.now_ms()),',
    count: 1,
    reason:
      'window-matched Option: `recorded.as_ref().map(|row| row.started_at)` is an Option chain over an already-read row; the fallback is the reconciler\'s own clock for a display timestamp.',
    owner: 'orchestrator completion steps',
  },
  {
    file: 'crates/orchestrator/src/shadow.rs',
    line_contains: 'let Ok(sessions) = self.manager.list_sessions(None) else {',
    count: 1,
    reason:
      'shadow reconcile: an unreadable session list REFUSES the reconcile with a typed ExecError::Internal (the else returns the error) — no absence is fabricated.',
    owner: 'orchestrator shadow',
  },
  {
    file: 'crates/orchestrator/src/shadow.rs',
    line_contains: 'let Ok(srow) = handle.row() else {',
    count: 1,
    reason:
      'shadow reconcile row classification: an unreadable session row is reported as a TYPED violation/reason in the reconcile report (fail-loud), never silently skipped.',
    owner: 'orchestrator shadow',
  },
  {
    file: 'crates/orchestrator/src/task_executor/recovery.rs',
    line_contains: 'let Ok(Some(handle)) = session.get_session(session_id) else {',
    count: 1,
    reason:
      'bare-admission wedge repair: a VANISHED session is a no-op by contract (nothing to repair); the wedge detector itself refuses typed when it cannot determine state, so the early return cannot hide a live wedge.',
    owner: 'orchestrator recovery',
  },
  {
    file: 'crates/orchestrator/src/tournament.rs',
    line_contains: 'if let Ok(rows) = OrchestratorRuntime::registry_rows(manager.clone(), handle.id(), run_family) {',
    count: 1,
    reason:
      'candidate discard cleanup (best-effort by documented contract: the caller records WHY on the tournament decision ledger row first); an unreadable registry leaves the row untouched rather than fabricating a cancellation.',
    owner: 'orchestrator tournament',
  },
  {
    file: 'crates/server/src/api/router.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'index-coverage snapshot projection: an unreadable session row omits the coverage snapshot from the response (the field is optional in the DTO); never a fabricated snapshot.',
    owner: 'server native surface',
  },
  {
    file: 'crates/server/src/native/attachment.rs',
    line_contains: 'Json(serde_json::to_value(&stored).unwrap_or(serde_json::Value::Null)).into_response()',
    count: 1,
    reason:
      'window-matched Option: the attachment REF was already durably stored by the preceding `put_attachment_ref` match; only its DTO serialization falls back to null (serialization of an owned struct cannot fail for real).',
    owner: 'server attachment surface',
  },
  {
    file: 'crates/server/src/native/proof.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'proof contract lookup: an unreadable contract row yields no verified-completion claims; the proof view then reports the criterion as unavailable, never verified.',
    owner: 'server proof surface',
  },
  {
    file: 'crates/server/src/native/session.rs',
    line_contains: '.unwrap_or(serde_json::Value::Null);',
    count: 1,
    reason:
      'progress-view projection: an unreadable progress view renders as an explicit null field in the read-model JSON; never fabricated progress numbers.',
    owner: 'server session surface',
  },
  {
    file: 'crates/server/src/native/session.rs',
    line_contains: 'let row = handle.row().ok();',
    count: 1,
    reason:
      'session-creation response: the row was JUST created; the projection falls back to the request values (title/created_ms placeholders below), and the durable row remains authoritative for every later read.',
    owner: 'server session surface',
  },
  {
    file: 'crates/server/src/native/session.rs',
    line_contains: '"title": row.as_ref().map(|r| r.title.clone()).unwrap_or(title),',
    count: 1,
    reason:
      'window-matched Option continuation of the just-created session row projection (same reviewed site): the fallback is the request title.',
    owner: 'server session surface',
  },
  {
    file: 'crates/server/src/native/session.rs',
    line_contains: '"created_ms": row.map(|r| r.created_ms).unwrap_or(0),',
    count: 1,
    reason:
      'window-matched Option continuation of the just-created session row projection: absent row timestamp renders 0 in a UI-safe placeholder, never a claim of durable state.',
    owner: 'server session surface',
  },
  {
    file: 'crates/server/src/native/task.rs',
    line_contains: '.unwrap_or(0),',
    count: 2,
    reason:
      'window-matched Result projection: `handle.row().map(|r| r.task_id.raw()).unwrap_or(0)` renders the response envelope of a task that was JUST admitted; task_id 0 is the documented "unresolved receipt" placeholder the client tolerates, and the durable receipt id is the authority.',
    owner: 'server task surface',
  },
  {
    file: 'crates/server/src/native/terminal.rs',
    line_contains: 'let after = q.after.unwrap_or(0);',
    count: 1,
    reason:
      'window-matched Option: query parameter `after` defaults to 0 (stream start); a malformed query is a client concern, not a durable read.',
    owner: 'server terminal surface',
  },
  {
    file: 'crates/server/src/native/usage.rs',
    line_contains: 'if let Ok(Some(cost)) = store.cost_task_row(sid, task.task_id) {',
    count: 1,
    reason:
      'usage aggregation: a task without a cost row contributes no spend to the fold; the page still reports its own row count and the strict billing fold refuses truncated history separately (the display sum is not the settlement authority).',
    owner: 'server usage surface',
  },
  {
    file: 'crates/scheduler/src/lib.rs',
    line_contains: '.unwrap_or_default();',
    count: 1,
    reason:
      'window-matched Option: `tasks.get_mut(&upstream_id).map(take dependents).unwrap_or_default()` — a missing upstream entry yields an empty dependent list; the scheduler\'s own admission validated every dependency edge before this point.',
    owner: 'scheduler core',
  },
  {
    file: 'crates/session/src/actor.rs',
    line_contains: '*self.shared.bridge.lock().unwrap_or_else(|p| p.into_inner()) = Some(bridge);',
    count: 1,
    reason:
      'poisoned-mutex recovery (into_inner), not an error→absence read: the lock payload is a JoinHandle slot, and a poisoned lock still stores the live bridge handle.',
    owner: 'session actor',
  },
  {
    file: 'crates/session/src/handle.rs',
    line_contains: 'let model = a.model.as_deref().unwrap_or(&row.model);',
    count: 1,
    reason:
      'window-matched Option: the per-turn model OVERRIDE (`a.model`) defaults to the durable session row model read with `?` on the line above; absence of the override is the documented semantics.',
    owner: 'session handle',
  },
  {
    file: 'crates/session/src/messages.rs',
    line_contains: 'let last_event_seq = self.last_event_seq()?.map(|s| s.raw() as i64).unwrap_or(0);',
    count: 1,
    reason:
      'session_state_view: the event sequence read propagates errors with `?`; the Option (no events yet) renders as 0 = "no events", the documented initial state of the view.',
    owner: 'session messages',
  },
  {
    file: 'crates/session/src/recovery.rs',
    line_contains: 'Err(_) => Ok(None),',
    count: 1,
    reason:
      'recovery probe: a failed legacy-row interpretation yields "no recoverable row" for THIS probe; the session-open path refuses typed when durable state cannot be read at all (the conservative bias of recovery).',
    owner: 'session recovery',
  },
  {
    file: 'crates/store/src/tasks.rs',
    line_contains: '.unwrap_or_default();',
    count: 1,
    reason:
      'window-matched Option: `conflicts.get_mut(index).map(take).unwrap_or_default()` — the index was just produced by the conflict scan above; an out-of-range index yields an empty CONFLICT MESSAGE, and the Conflict error is still returned.',
    owner: 'store writer tasks',
  },
  {
    file: 'crates/agent/src/runtime/goal.rs',
    line_contains: 'let title = handle.title().unwrap_or_default();',
    count: 1,
    reason:
      'goal label detection: the session title is only a comparison value for detecting a label/empty goal; an unreadable title yields the empty string, so the check treats the goal as needing heal from the durable ledger and never fabricates an objective.',
    owner: 'agent goal authority',
  },
  {
    file: 'crates/agent/src/runtime/goal.rs',
    line_contains: 'clear_label_task_goal(handle, &handle.title().unwrap_or_default());',
    count: 1,
    reason:
      'clear_label_task_goal call after a typed goal_unavailable_after_redaction: the title fallback only decides whether a stale TASK label equals the title; an unreadable title clears nothing, and the typed error above is returned unchanged.',
    owner: 'agent goal authority',
  },
  {
    file: 'crates/agent/src/runtime/goal.rs',
    line_contains: 'let Ok(task_id) = handle.task_id() else {',
    count: 1,
    reason:
      'clear_label_task_goal is a best-effort display cleanup of the task label: an unreadable task id returns without clearing; it never gates completion and the durable goal/criteria ledger remains authoritative.',
    owner: 'agent goal authority',
  },
  {
    file: 'crates/agent/src/runtime/goal.rs',
    line_contains: 'let Ok(Some(task)) = handle.get_task(task_id) else {',
    count: 1,
    reason:
      'same best-effort label cleanup: an unreadable/absent task row leaves the label untouched; the durable task row is read typed by every completion path.',
    owner: 'agent goal authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: 'let page = handle.messages_before(None, 8).ok()?;',
    count: 1,
    reason:
      'latest_user_prompt_message is a display/context helper returning Option; an unreadable transcript page yields no prompt hint, and instruction resolution still reads the durable bundle.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: '.or_else(|| handle.title().ok())',
    count: 1,
    reason:
      'prompt hint falls back from the newest user message to the session title; both are display-only content for instruction matching, never an identity or authority read.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: '.unwrap_or_default();',
    count: 1,
    reason:
      'empty prompt hint when neither the latest message nor the title is readable; the compiler still resolves the durable instruction bundle.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: '.ok()',
    count: 2,
    reason:
      'Two optional instruction-context reads: the shadow liveness probe treats an unreadable active root as no live shadow (so the ordinary captured bundle is pinned), and the captured binding read yields no pinned bundle on failure while the resolver falls back to the durable bundle. Neither is an identity claim.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: 'if let Ok(row) = handle.row() {',
    count: 1,
    reason:
      'workspace memory facts are an optional enrichment of the review context; an unreadable session row skips the workspace scope while the session-scope facts above are still rendered.',
    owner: 'agent retrieval authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: 'let title = handle.title().unwrap_or_default();',
    count: 1,
    reason:
      'the title is a comparison value for the label-goal heal; an unreadable title cannot match a non-empty label, so the heal is skipped and the durable goal is left unchanged (secret detection still runs independently).',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: 'let goal_fact = handle.memory_fact_get("task", "goal").unwrap_or_default();',
    count: 1,
    reason:
      'goal memory fact read used to detect drift; an unreadable read is treated as not-yet-matching and the canonical fact is upserted, never a completion decision.',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/settlement.rs',
    line_contains: 'let criteria_fact = handle.memory_fact_get("criteria", "0").unwrap_or_default();',
    count: 1,
    reason:
      'criteria memory fact read used to detect drift; an unreadable read is treated as not-yet-matching and the canonical fact is upserted, never a completion decision.',
    owner: 'agent settlement authority',
  },
  {
    file: 'crates/agent/src/runtime/test_rebind.rs',
    line_contains: 'if let Ok(edges) = git_rename_edges(supervisor.clone(), root, handle.id()).await {',
    count: 1,
    reason:
      'git rename edges are optional rebind evidence; an unreadable supervisor/git query yields no rename edges and the evidence reports its own basis; changed test-bearing paths are still processed.',
    owner: 'agent test rebind',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive.rs',
    line_contains: 'view.head.goal.is_empty() || view.head.goal == handle.title().unwrap_or_default();',
    count: 1,
    reason:
      'legacy head label test: the title is a comparison value; an unreadable title yields no label match, so the head is only rewritten when it is empty or carries a secret (the secret test is independent).',
    owner: 'agent turn drive',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive_phases.rs',
    line_contains: 'let Ok(Some(raw)) = handle.memory_fact_get("session_settings", "contextWindow") else {',
    count: 1,
    reason:
      'durable per-session context-window override: an unreadable/missing setting leaves the model window unchanged; the override can only lower the budget.',
    owner: 'agent turn planning',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let row = handle.row().ok()?;',
    count: 1,
    reason:
      'workspace open helper: an unreadable session row yields no workspace handle; callers treat None as cannot-observe and refuse the relevant proof/change claim.',
    owner: 'agent turn state',
  },
  {
    file: 'crates/cli/src/daemon/semantic.rs',
    line_contains: 'Err(_) => {}',
    count: 2,
    reason:
      'bounded LSP readiness polling: a failed probe is retried; at the deadline readiness proceeds to the real request, whose own typed result is authoritative (no fabricated success).',
    owner: 'cli semantic readiness',
  },
  {
    file: 'crates/cli/src/supply.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'approval lookup for qualification: errors that are not hard failures/Denials mean no token; assess_qualification then refuses when approval is required (conservative).',
    owner: 'cli supply actions',
  },
  {
    file: 'crates/cli/src/supply.rs',
    line_contains: 'Err(_) => {}',
    count: 1,
    reason:
      'classification approval loop: a non-hard, non-Denied authorization error contributes no token, so the decision cannot claim approval.',
    owner: 'cli supply actions',
  },
  {
    file: 'crates/cloud/src/entitlements.rs',
    line_contains: 'let started_ms = current.as_ref().map(|s| s.started_ms).unwrap_or(now);',
    count: 1,
    reason:
      'subscription ensure: absence of an existing subscription starts the period now; a store error propagates with `?` above, so `now` is only the documented first-subscription start.',
    owner: 'cloud entitlements',
  },
  {
    file: 'crates/foundation/src/providers.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'proxy URL parse: a malformed proxy URL means no proxy rule is added; the checked transport then refuses the proxy/connection rather than bypassing policy.',
    owner: 'foundation providers',
  },
  {
    file: 'crates/impact/src/lsp.rs',
    line_contains: 'let Ok(symbols) = session.document_symbols(&absolute) else {',
    count: 1,
    reason:
      'document symbols probe: a failed request marks the impact stats degraded and continues; degraded output is surfaced, never presented as complete.',
    owner: 'impact lsp',
  },
  {
    file: 'crates/impact/src/lsp.rs',
    line_contains: 'if let Ok(locations) = session.references(&absolute, symbol.line, symbol.character) {',
    count: 1,
    reason:
      'references request failure is recorded as degraded (stats.degraded) and the scan continues; no fabricated edges.',
    owner: 'impact lsp',
  },
  {
    file: 'crates/impact/src/lsp.rs',
    line_contains: 'let Ok(incoming) = session.incoming_calls(&item) else {',
    count: 1,
    reason:
      'incoming-calls failure marks degraded and continues; the impact graph surfaces degradation instead of claiming completeness.',
    owner: 'impact lsp',
  },
  {
    file: 'crates/memory/src/management.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'MemoryRecordStore constructor: an unreadable session row yields workspace scope 0; session-scope reads stay keyed by the session id, and workspace facts require the later typed workspace resolution.',
    owner: 'memory management',
  },
  {
    file: 'crates/memory/src/management.rs',
    line_contains: '.unwrap_or(u64::MAX)),',
    count: 1,
    reason:
      'u64 conversion of a count saturates at u64::MAX; it is an overflow bound, not an error→absence read.',
    owner: 'memory management',
  },
  {
    file: 'crates/orchestrator/src/runtime_handoff.rs',
    line_contains: 'let Ok(Some(handle)) = self.manager.get_session(parent) else {',
    count: 1,
    reason:
      'heal_pending_child_handoffs is documented best-effort: a vanished/unreadable parent logs and skips; the run\'s own durable rows remain the authority and the next re-attach retries.',
    owner: 'orchestrator child recovery',
  },
  {
    file: 'crates/orchestrator/src/runtime_helpers.rs',
    line_contains: 'let handle = manager.get_session(session_id).ok().flatten()?;',
    count: 1,
    reason:
      'child_task_budget_cap mirror derivation: documented derive-only projection of the task row token cap (None = no task row / unlimited); the durable budget patch already landed on the child through the enqueued+acked control, enforcement lives in the child session, and registry reads re-derive through derive_child_projection.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime_helpers.rs',
    line_contains: 'let task_id = handle.task_id().ok()?;',
    count: 1,
    reason:
      'child_task_budget_cap mirror derivation (id leg): an unreadable id yields no mirror derivation; never a fabricated cap, never an uncapped claim.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime_helpers.rs',
    line_contains: '.and_then(|id| manager.get_session(id).ok().flatten())',
    count: 1,
    reason:
      'child_execution_phase mirror projection: documented presentational projection (Planning when no drive-state row exists); an unreadable session derives the conservative default, the durable drive-state row is re-derived on every registry read, and the phase is never a lifecycle transition.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime_helpers.rs',
    line_contains: '.and_then(|h| h.orchestrator_drive_state_get().ok())',
    count: 1,
    reason:
      'child_execution_phase mirror projection (drive-state leg): same reviewed presentational semantics as the session leg above; re-derived on every registry read.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/runtime_helpers.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'child_task_budget_cap mirror derivation (task row leg): an unreadable task row yields no mirror value; the durable child-side budget remains the enforcement authority and registry reads re-derive.',
    owner: 'orchestrator child registry projection',
  },
  {
    file: 'crates/orchestrator/src/task_executor/mod.rs',
    line_contains: 'task_id: handle.task_id().ok().map(|t| t.raw()),',
    count: 1,
    reason:
      'run receipt projection: an unreadable task id renders null in the refused-isolation run row; the task id is not the enforcement authority and the row is otherwise durable.',
    owner: 'orchestrator task executor',
  },
  {
    file: 'crates/server/src/native/foundation.rs',
    line_contains: 'let path = std::env::current_exe().ok()?;',
    count: 1,
    reason:
      'binary checksum helper: current_exe unavailability yields no checksum (None) rather than a fabricated digest; the caller reports checksum unavailable.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation.rs',
    line_contains: 'let metadata = std::fs::metadata(&path).ok()?;',
    count: 1,
    reason:
      'binary checksum helper: metadata unavailability yields no checksum (None) rather than a fabricated digest; the caller reports checksum unavailable.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'task ledger projection: an unreadable/absent ledger renders the default empty ledger for this read model; the authoritative task row and ledger writes stay typed elsewhere.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_ext.rs',
    line_contains: 'if let Ok(Some(root)) = state.deps.session.resolve_workspace_root(handle.id()) {',
    count: 1,
    reason:
      'workspace skill registry discovery: an unavailable workspace root yields no workspace-installed skill candidates; installed records remain and no skill is invented.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_ext.rs',
    line_contains: 'source.row().map(|r| r.title).unwrap_or_default()',
    count: 1,
    reason:
      'fork default title: an unreadable source row falls back to the literal "(fork)" label; fork_session then performs the typed durable operation.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_ext.rs',
    line_contains: 'Err(_) => None,',
    count: 1,
    reason:
      'checkpoint restore preimage hash: any read failure yields None, which is compared against the expected hash and REFUSES with checkpoint_conflict.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_ext.rs',
    line_contains: 'for row in handle.checkpoints_of().ok().unwrap_or_default() {',
    count: 1,
    reason:
      'review-comment latest-change map: an unreadable checkpoint list yields no change hashes; comments still render without a fabricated identity.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_ext.rs',
    line_contains: '.ok()',
    count: 3,
    reason:
      'Three optional export/projection reads: workspace root resolution for the latest diff, workspace open for that diff, and the snapshot diff lookup itself. Absence renders no diff text; the checkpoint identity list and transcript are read typed, so no change identity is fabricated.',
    owner: 'server foundation surface',
  },
  {
    file: 'crates/server/src/native/foundation_r8.rs',
    line_contains: 'Ok(head) => head.map(|seq| seq.raw()).unwrap_or(0),',
    count: 1,
    reason:
      'event window anchor: `last_event_seq` propagates errors typed; Ok(None) means no events yet, so 0 is the documented empty head.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/instructions.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'optional instruction binding capture read: absence means no captured bundle; the resolver re-resolves the durable bundle.',
    owner: 'server instructions surface',
  },
  {
    file: 'crates/server/src/native/instructions.rs',
    line_contains: '.unwrap_or_else(|| handle.title().unwrap_or_default());',
    count: 2,
    reason:
      'instruction preview prompt: the session title is the documented display fallback when the request carries no explicit prompt; never an identity/authority input.',
    owner: 'server instructions surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: 'Ok(latest) => latest.map(|m| m.revision).unwrap_or(0),',
    count: 1,
    reason:
      'manifest revision check: Ok(None) from a typed read means no manifest exists yet, so 0 is the empty revision base.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: '.and_then(|f| f.context.latest(session_u64(handle.id())).ok().flatten())',
    count: 1,
    reason:
      'optional latest manifest for the expected-revision comparison; a missing manifest leaves the check to the None arm, and store errors are handled in the neighboring typed branch.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: 'req.target_tokens.unwrap_or(0) as i64,',
    count: 1,
    reason:
      'target_tokens is an optional request field; 0 is the documented "no explicit target" value passed to the typed begin operation.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: 'if let Ok(Some(current)) = handle.compaction_operation(&operation_id) {',
    count: 1,
    reason:
      'late terminal-race re-read: if the operation row is unreadable, the response reports the already-returned cancel outcome without fabricating an operation body.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: 'let mut newest_seq = base_seq.unwrap_or(0);',
    count: 1,
    reason:
      'base_seq absence means the window starts at the beginning; 0 is the documented initial sequence.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: 'let auto_compaction = handle.runtime_compaction().ok().flatten().map(|marker| {',
    count: 1,
    reason:
      'runtime compaction marker optional; absence means no auto-compaction marker, never fabricated progress.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/longtask.rs',
    line_contains: '.ok()',
    count: 3,
    reason:
      'Three optional compaction projections/settings: the manual-compaction baseline ledger, the latest context manifest, and the session compaction model / configured reduction floor. An unreadable optional value falls back to the documented request/durable default, and the durable transcript/manifest reads remain the authority.',
    owner: 'server compaction surface',
  },
  {
    file: 'crates/server/src/native/plan_api.rs',
    line_contains: '"artifactId": format!("plan-{}-v{}", handle.id().raw(), meta.get("version").and_then(|v| v.as_u64()).unwrap_or(0)),',
    count: 1,
    reason:
      'artifact id version component: a plan artifact without a numeric version renders version 0; the artifact body itself is returned unchanged and approval is keyed by the frozen artifact.',
    owner: 'server plan surface',
  },
  {
    file: 'crates/server/src/native/usage.rs',
    line_contains: '"budget" => entry.0 = value.parse::<i64>().ok(),',
    count: 1,
    reason:
      'session budget/spent facts are model/tool-writable data; an unparsable value is treated as absent in the usage display, and the billing fold remains the settlement authority.',
    owner: 'server usage surface',
  },
  {
    file: 'crates/server/src/native/worktrees_api.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'worktree transfer target check: an unreadable target session is refused as session_not_found (no transfer is attempted), never treated as an existing owner.',
    owner: 'server worktree surface',
  },
  {
    file: 'crates/session/src/manager.rs',
    line_contains: 'if verified.get(&handle.id()).copied().unwrap_or(0) >= max_seq {',
    count: 1,
    reason:
      'ledger verification watermark is an in-memory fast path; a missing entry starts at 0 and the durable ledger rows below remain authoritative.',
    owner: 'session manager',
  },
  {
    file: 'crates/session/src/manager.rs',
    line_contains: 'let from = verified.get(&handle.id()).copied().unwrap_or(0);',
    count: 1,
    reason:
      'same in-memory verification watermark: absence starts from the beginning, and the durable ledger read is the authority.',
    owner: 'session manager',
  },
  {
    file: 'crates/site/src/auth.rs',
    line_contains: 'hash_password("dummy password for timing equalization").unwrap_or_default()',
    count: 1,
    reason:
      'timing-equalization dummy hash: the hash is fixed content and only burns comparable time; if hashing were to fail, an empty hash still fails verification and no credential decision changes.',
    owner: 'site auth',
  },
  {
    file: 'crates/store/src/queue_store.rs',
    line_contains: 'row.get::<_, i64>(0).unwrap_or(-1)',
    count: 1,
    reason:
      'corruption error message: a failed seq extraction renders -1 inside the Corrupt diagnostic only; the error itself is still returned.',
    owner: 'store queue',
  },
  {
    file: 'crates/terminal/src/sandbox/macos.rs',
    line_contains: 'Err(_) => false,',
    count: 1,
    reason:
      'executability probe: metadata failure means not executable (false), the conservative direction for confinement checks.',
    owner: 'terminal macos sandbox',
  },
];

// (1) collapse-on-authority-call
const AUTHORITY_RECEIVER =
  /(?:handle|store|plane|session)\.[a-z_]+\(|\.memory_facts\(|\.list_[a-z_]*\(|\.get_[a-z_]*\(|\.task_id\(|\.row\(|\.checkpoints\(|\.token\(|\.worker\(|\.lease[a-z_]*\(|resolver\.resolve\(/;
// `.unwrap_or_else`/`.map_or` fabricate defaults exactly like unwrap_or.
const COLLAPSE = /\.ok\(\)|\.unwrap_or_default\(\)|\.unwrap_or\(|\.unwrap_or_else\(|\.map_or\(/;
// (2) value-pattern binding on a Result (`let Ok(` / `if let Ok(`).
const LET_OK = /let\s+Ok\(/;
// (3) bare err-arm: `Err(_) => None/vec![]/false/0/...` — fabricating a
//     default in an authority path is the same failure mode as a bare None.
const DEFAULT_VALUE =
  '(?:None|Ok\\(None\\)|Vec::new\\(\\)|vec!\\[\\s*\\]|String::new\\(\\)|false|0)';
const BARE_ERR_ARM = new RegExp(
  'Err\\(_\\s*\\)\\s*=>\\s*(?:' +
    `\\{\\s*\\}\\s*,?` +
    `|\\{\\s*return\\s+${DEFAULT_VALUE};?\\s*\\}\\s*,?` +
    `|\\{\\s*${DEFAULT_VALUE}\\s*;?\\s*\\}\\s*,?` +
    `|${DEFAULT_VALUE}\\s*,?` +
    `|return\\s+${DEFAULT_VALUE};?` +
    ')\\s*$',
);

// --------------------------------------------------------- lexing / masking

/// Byte mask: true = semantic code (comments and string/char literals masked
/// out). Mirror of the tests/static-authority scanner so the JS gate and the
/// Rust gate agree byte-for-byte on what is "source text".
/// The scanning alphabet: 1 JS char == 1 UTF-8 byte (latin1 view of the
/// file), so every index in this script is a BYTE offset and agrees with the
/// masks and with the Rust static-authority scanner. Non-ASCII content is
/// irrelevant to the (ASCII) patterns; display text is re-decoded from the
/// original bytes.
function latin(text) {
  return Buffer.from(text, 'utf8').toString('latin1');
}

function codeMask(src) {
  const b = Buffer.from(src, 'latin1');
  const n = b.length;
  const code = new Uint8Array(n).fill(1);
  const mask = (from, to) => {
    const t = Math.min(to, n);
    code.fill(0, from, t);
  };
  let i = 0;
  while (i < n) {
    if (b[i] === 0x2f && i + 1 < n && b[i + 1] === 0x2f) {
      let j = i;
      while (j < n && b[j] !== 0x0a) j++;
      mask(i, j);
      i = j;
    } else if (b[i] === 0x2f && i + 1 < n && b[i + 1] === 0x2a) {
      let j = i + 2;
      while (j < n && !(b[j] === 0x2a && j + 1 < n && b[j + 1] === 0x2f)) j++;
      if (j < n) j += 2;
      mask(i, j);
      i = j;
    } else if (b[i] === 0x72 || (b[i] === 0x62 && i + 1 < n && b[i + 1] === 0x72)) {
      let k = i + (b[i] === 0x62 ? 2 : 1);
      while (k < n && b[k] === 0x23) k++;
      if (k < n && b[k] === 0x22) {
        const prefix = b[i] === 0x62 ? 2 : 1;
        let q = i + prefix;
        while (q < n && b[q] === 0x23) q++;
        const hashes = q - (i + prefix);
        let j = q + 1;
        while (j < n) {
          if (b[j] === 0x22) {
            let h = 0;
            while (j + 1 + h < n && h < hashes && b[j + 1 + h] === 0x23) h++;
            if (h === hashes) {
              j += 1 + hashes;
              break;
            }
          }
          j++;
        }
        mask(i, j);
        i = j;
      } else {
        i++;
      }
    } else if (b[i] === 0x22) {
      let j = i + 1;
      while (j < n) {
        if (b[j] === 0x5c && j + 1 < n) j += 2;
        else if (b[j] === 0x22) {
          j++;
          break;
        } else j++;
      }
      mask(i, j);
      i = j;
    } else if (b[i] === 0x27) {
      // char literal (masked) vs lifetime (kept): only mask when a closing
      // quote exists nearby.
      let j = i + 1;
      let closed = false;
      while (j < n && j <= i + 12) {
        if (b[j] === 0x5c && j + 1 < n) {
          j += 2;
          continue;
        }
        if (b[j] === 0x27) {
          closed = true;
          break;
        }
        j++;
      }
      if (closed) {
        mask(i, j + 1);
        i = j + 1;
      } else {
        i++;
      }
    } else {
      i++;
    }
  }
  return code;
}

// Three-valued `cfg(…)` evaluation, ported from tests/static-authority.
const TV = { T: 0, F: 1, U: 2 };

function cfgEval(body, test, unknown) {
  const bytes = Buffer.from(body, 'utf8');
  let pos = 0;
  const identEnd = (p) => {
    while (p < bytes.length && /[A-Za-z0-9_]/.test(String.fromCharCode(bytes[p]))) p++;
    return p;
  };
  const parse = () => {
    if (bytes.slice(pos, pos + 4).toString() === 'not(') {
      pos += 4;
      const v = parse();
      if (v === null) return null;
      if (bytes[pos] !== 0x29) return null;
      pos += 1;
      return v === TV.T ? TV.F : v === TV.F ? TV.T : TV.U;
    }
    for (const op of ['all(', 'any(']) {
      if (bytes.slice(pos, pos + 4).toString() === op) {
        pos += 4;
        const vals = [];
        for (;;) {
          const v = parse();
          if (v === null) return null;
          vals.push(v);
          if (pos >= bytes.length) return null;
          if (bytes[pos] === 0x2c) pos += 1;
          else if (bytes[pos] === 0x29) {
            pos += 1;
            break;
          } else return null;
        }
        const isAll = op === 'all(';
        const sawT = vals.includes(TV.T);
        const sawF = vals.includes(TV.F);
        const sawU = vals.includes(TV.U);
        if (isAll) return sawF ? TV.F : sawU ? TV.U : TV.T;
        return sawT ? TV.T : sawU ? TV.U : TV.F;
      }
    }
    const end = identEnd(pos);
    if (end === pos) return null;
    const ident = bytes.slice(pos, end).toString();
    pos = end;
    if (bytes[pos] === 0x3d) {
      pos += 1;
      if (bytes[pos] !== 0x22) return null;
      pos += 1;
      while (pos < bytes.length && bytes[pos] !== 0x22) pos++;
      if (pos >= bytes.length) return null;
      pos += 1;
    }
    if (ident === 'test') return test ? TV.T : TV.F;
    return unknown ? TV.T : TV.F;
  };
  const v = parse();
  if (v === null || pos !== bytes.length) return null;
  return v;
}

function cfgMentionsTest(body) {
  return /\btest\b/.test(body);
}

function isTestGated(attrBody) {
  if (!cfgMentionsTest(attrBody)) return false;
  const compact = attrBody.replace(/\s+/g, '');
  return (
    cfgEval(compact, false, true) === TV.F && cfgEval(compact, false, false) === TV.F
  );
}

/**
 * Kept (production) byte mask: everything outside `#[cfg(...)]`-gated items
 * that can never exist in a non-test build. `code` masks comments/strings.
 */
function keptMask(src, code) {
  const b = Buffer.from(src, 'latin1');
  const n = b.length;
  const kept = new Uint8Array(n).fill(1);
  const drop = (from, to) => {
    kept.fill(0, from, Math.min(to, n));
  };
  let i = 0;
  while (i + 6 <= n) {
    if (b.slice(i, i + 6).toString() === '#[cfg(' && code[i]) {
      let end = null;
      for (let j = i + 6; j + 2 <= n; j++) {
        if (b[j] === 0x29 && b[j + 1] === 0x5d && code[j] && code[j + 1]) {
          end = j + 2;
          break;
        }
      }
      if (end === null) break;
      const body = b.slice(i + 6, end - 2).toString('latin1');
      if (!isTestGated(body)) {
        i = end;
        continue;
      }
      // skip any further attributes attached to the same item
      let k = end;
      for (;;) {
        while (k < n && !code[k]) k++;
        if (k < n && b[k] === 0x23 && b[k + 1] === 0x5b) {
          let depth = 0;
          let m = k;
          while (m < n) {
            if (code[m]) {
              if (b[m] === 0x5b) depth++;
              else if (b[m] === 0x5d) {
                depth--;
                if (depth === 0) {
                  m++;
                  break;
                }
              }
            }
            m++;
          }
          k = m;
        } else break;
      }
      // locate the item: '{' block or ';' statement at depth 0
      let depth = 0;
      let itemEnd = null;
      while (k < n) {
        if (!code[k]) {
          k++;
          continue;
        }
        const ch = b[k];
        if (ch === 0x28 || ch === 0x5b) depth++;
        else if (ch === 0x29 || ch === 0x5d) depth = Math.max(0, depth - 1);
        else if (ch === 0x7b && depth === 0) {
          let d = 0;
          let m = k;
          while (m < n) {
            if (code[m]) {
              if (b[m] === 0x7b) d++;
              else if (b[m] === 0x7d) {
                d--;
                if (d === 0) {
                  itemEnd = m + 1;
                  break;
                }
              }
            }
            m++;
          }
          break;
        } else if (ch === 0x3b && depth === 0) {
          itemEnd = k + 1;
          break;
        }
        k++;
      }
      if (itemEnd !== null) drop(i, itemEnd);
      i = itemEnd !== null ? itemEnd : end;
    } else {
      i++;
    }
  }
  return kept;
}

// --------------------------------------------- test-only out-of-line modules

function normalizeRel(rel) {
  const parts = [];
  for (const seg of rel.replace(/\\/g, '/').split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      if (parts.length > 0) parts.pop();
      else parts.push('..');
    } else parts.push(seg);
  }
  return parts.join('/');
}

/** `(offset of 'mod ', stem)` for every out-of-line `mod <stem>;`. */
function outOfLineMods(src) {
  const code = codeMask(src);
  const b = Buffer.from(src, 'latin1');
  const out = [];
  for (const at of codeNeedleOffsets(src, 'mod ', code)) {
    if (at > 0) {
      const prev = b[at - 1];
      if ((prev >= 0x30 && prev <= 0x39) || (prev >= 0x41 && prev <= 0x5a) ||
          (prev >= 0x61 && prev <= 0x7a) || prev === 0x5f) {
        continue;
      }
    }
    let end = at + 4;
    while (end < b.length && /[A-Za-z0-9_]/.test(String.fromCharCode(b[end]))) end++;
    if (end === at + 4) continue;
    const stem = b.slice(at + 4, end).toString();
    let k = end;
    while (k < b.length && /\s/.test(String.fromCharCode(b[k]))) k++;
    if (b[k] === 0x3b) out.push([at, stem]);
  }
  return out;
}

function codeNeedleOffsets(src, needle, code) {
  const c = code ?? codeMask(src);
  const out = [];
  let pos = 0;
  for (;;) {
    const at = src.indexOf(needle, pos);
    if (at === -1) break;
    let ok = true;
    for (let i = at; i < at + needle.length; i++) if (!c[i]) ok = false;
    if (ok) out.push(at);
    pos = at + needle.length;
  }
  return out;
}

/** The contiguous `#[...]` attribute run immediately before `at`. */
function precedingAttributes(src, at) {
  let end = src.slice(0, at).replace(/\s+$/, '').length;
  let out = '';
  for (;;) {
    const s = src.slice(0, end);
    if (s.length === 0 || s[s.length - 1] !== ']') break;
    const bytes = Buffer.from(s, 'latin1');
    let depth = 0;
    let start = null;
    for (let i = bytes.length; i > 0; i--) {
      const ch = bytes[i - 1];
      if (ch === 0x5d) depth++;
      else if (ch === 0x5b) {
        depth--;
        if (depth === 0) {
          start = i - 1;
          break;
        }
      }
    }
    if (start === null || start === 0 || bytes[start - 1] !== 0x23) break;
    out = src.slice(start - 1, end) + out;
    end = src.slice(0, start - 1).replace(/\s+$/, '').length;
  }
  return out;
}

/** Attribute run of the declaration whose `at` points at `mod` (visibility
 * qualifiers skipped). */
function declarationAttrs(src, at) {
  const bytes = Buffer.from(src, 'latin1');
  let end = src.slice(0, at).replace(/\s+$/, '').length;
  const trailingWord = (j) => {
    let k = j;
    while (k > 0 && /\s/.test(String.fromCharCode(bytes[k - 1]))) k--;
    const wordEnd = k;
    while (k > 0 && /[A-Za-z0-9_]/.test(String.fromCharCode(bytes[k - 1]))) k--;
    return [k, wordEnd];
  };
  if (end > 0 && bytes[end - 1] === 0x29) {
    let depth = 0;
    let open = null;
    for (let i = end; i > 0; i--) {
      const ch = bytes[i - 1];
      if (ch === 0x29) depth++;
      else if (ch === 0x28) {
        depth--;
        if (depth === 0) {
          open = i - 1;
          break;
        }
      }
    }
    if (open !== null) {
      const [k, wordEnd] = trailingWord(open);
      if (bytes.slice(k, wordEnd).toString() === 'pub') end = k;
    }
  } else {
    const [k, wordEnd] = trailingWord(end);
    if (bytes.slice(k, wordEnd).toString() === 'pub') end = k;
  }
  const tail = src.slice(0, end).replace(/\s+$/, '');
  return precedingAttributes(src, tail.length);
}

function attrPathValue(attrs) {
  const at = attrs.indexOf('path = "');
  if (at === -1) return null;
  const rest = attrs.slice(at + 8);
  const end = rest.indexOf('"');
  if (end === -1) return null;
  return normalizeRel(rest.slice(0, end));
}

function declaredFilePath(declaringRel, stem, attrs) {
  const slash = declaringRel.lastIndexOf('/');
  if (slash === -1) return normalizeRel(`${stem}.rs`);
  const dir = declaringRel.slice(0, slash);
  const name = declaringRel.slice(slash + 1);
  const path = attrPathValue(attrs);
  if (path !== null) return normalizeRel(`${dir}/${path}`);
  if (name === 'main.rs' || name === 'lib.rs' || name === 'mod.rs') {
    return normalizeRel(`${dir}/${stem}.rs`);
  }
  const moduleDir = name.endsWith('.rs') ? name.slice(0, -3) : name;
  return normalizeRel(`${dir}/${moduleDir}/${stem}.rs`);
}

function hasTestGatedCfg(attrs) {
  let rest = attrs;
  for (;;) {
    const at = rest.indexOf('#[cfg(');
    if (at === -1) return false;
    const bodyStart = at + 6;
    const end = rest.slice(bodyStart).indexOf(')]');
    if (end === -1) return false;
    const body = rest.slice(bodyStart, bodyStart + end).replace(/\s+/g, '');
    if (isTestGated(body)) return true;
    rest = rest.slice(bodyStart + end + 2);
  }
}

/** Files declared ONLY as test-gated out-of-line modules (over `files`). */
function testOnlyOutOfLineModules(files) {
  const states = new Map();
  for (const file of files) {
    const rel = normalizeRel(file.path);
    for (const [at, stem] of outOfLineMods(file.text)) {
      const attrs = declarationAttrs(file.text, at);
      const declared = declaredFilePath(rel, stem, attrs);
      const entry = states.get(declared) ?? { test: false, production: false };
      if (hasTestGatedCfg(attrs)) entry.test = true;
      else entry.production = true;
      states.set(declared, entry);
    }
  }
  const out = new Set();
  for (const [file, state] of states) {
    if (state.test && !state.production) out.add(file);
  }
  return out;
}

function isTestPath(rel) {
  const norm = normalizeRel(rel);
  const segments = norm.split('/');
  return (
    norm.endsWith('_tests.rs') ||
    norm.endsWith('/tests.rs') ||
    segments.includes('tests') ||
    segments.includes('test') ||
    segments.some((s) => s.endsWith('_tests'))
  );
}

// --------------------------------------------------------------- tree walk

function walkProductionRust(dir, out, root) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (entry.name.startsWith('.')) continue;
      walkProductionRust(path, out, root);
    } else if (entry.name.endsWith('.rs')) {
      out.push(normalizeRel(relative(root, path)));
    }
  }
}

function productionPaths(root = ROOT) {
  const cratesDir = resolve(root, 'crates');
  const found = [];
  for (const entry of readdirSync(cratesDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    // `tests/`, `examples/`, `benches/` subtrees are test/tooling code by
    // Cargo layout and never production sources.
    walkProductionRust(join(cratesDir, entry.name, 'src'), found, root);
  }
  return found.sort();
}

// --------------------------------------------------------------- scanning

/**
 * Prepare one file for scanning: comment/string mask + kept (non-test-gated)
 * mask. `testFile` exempts the whole file.
 */
function prepare(path, text, testFile) {
  // Scan on the BYTE-alphabet view; keep the original bytes for display.
  const scrubbed = latin(text);
  const code = codeMask(scrubbed);
  const kept = testFile ? new Uint8Array(scrubbed.length) : keptMask(scrubbed, code);
  return { path, text, scrubbed, code, kept };
}

/// Decode a byte range of one prepared file back to display text.
function displaySlice(file, from, to) {
  return Buffer.from(file.scrubbed.slice(from, to), 'latin1').toString('utf8');
}

function rangeKept(file, at, len) {
  for (let i = at; i < at + len; i++) {
    if (!file.code[i] || !file.kept[i]) return false;
  }
  return true;
}

function trimLine(file, at) {
  const text = file.scrubbed;
  const start = text.lastIndexOf('\n', at) + 1;
  let end = text.indexOf('\n', at);
  if (end === -1) end = text.length;
  const raw = displaySlice(file, start, end).trim();
  return raw;
}

function lineStarts(text) {
  const starts = [0];
  for (let i = 0; i < text.length; i++) if (text[i] === '\n') starts.push(i + 1);
  return starts;
}

/**
 * All error→absence hits of one prepared production file. Positions are
 * validated against BOTH masks (a marker inside a comment, a string or a
 * test-gated item can never count).
 */
function scanText(file) {
  const hits = [];
  const { code, kept } = file;
  const text = file.scrubbed;
  const starts = lineStarts(text);
  const lines = text.split('\n');
  const firstMatchKept = (re, haystack, base) => {
    const m = re.exec(haystack);
    if (m === null) return null;
    const at = base + m.index;
    for (let i = at; i < at + m[0].length; i++) {
      if (!code[i] || !kept[i]) return null;
    }
    return at;
  };
  for (let index = 0; index < lines.length; index++) {
    const line = lines[index];
    const base = starts[index];
    if (line.trim().startsWith('//')) continue;
    // (1) collapse on an authority receiver (same line or the two-line
    // window above it, so a multi-line receiver chain is covered).
    const collapseAt = firstMatchKept(COLLAPSE, line, base);
    if (collapseAt !== null) {
      const sameLine = firstMatchKept(AUTHORITY_RECEIVER, line, base);
      let windowed = null;
      for (let w = Math.max(0, index - 2); w < index; w++) {
        const at = firstMatchKept(AUTHORITY_RECEIVER, lines[w], starts[w]);
        if (at !== null) {
          windowed = at;
          break;
        }
      }
      if (sameLine !== null || windowed !== null) {
        hits.push({
          path: file.path,
          line: index + 1,
          kind: 'collapse',
          text: displaySlice(file, base, base + line.length).trim(),
        });
      }
    }
    // (2) value-pattern binding on an authority Result.
    const letOkAt = firstMatchKept(LET_OK, line, base);
    if (letOkAt !== null && firstMatchKept(AUTHORITY_RECEIVER, line, base) !== null) {
      hits.push({
        path: file.path,
        line: index + 1,
        kind: 'let-ok',
        text: displaySlice(file, base, base + line.length).trim(),
      });
    }
    // (3) bare Err(_) default arm (receiver-free: the arm body itself is
    // the transformation).
    if (firstMatchKept(BARE_ERR_ARM, line, base) !== null) {
      hits.push({
        path: file.path,
        line: index + 1,
        kind: 'bare-err-arm',
        text: displaySlice(file, base, base + line.length).trim(),
      });
    }
  }
  return hits;
}

/**
 * Scan a list of { path, text } files: test-only files (path heuristics or
 * test-gated-only out-of-line declarations) are excluded, every other file
 * is scanned for error→absence transformations. No file list, no
 * annotation. Returns { scanned, hits }.
 */
function scanFiles(files) {
  // The declaration analysis runs on the same byte alphabet as the masks.
  const testOnly = testOnlyOutOfLineModules(
    files.map((file) => ({ path: file.path, text: latin(file.text) })),
  );
  const hits = [];
  let scanned = 0;
  for (const file of files) {
    if (isTestPath(file.path) || testOnly.has(normalizeRel(file.path))) continue;
    scanned += 1;
    hits.push(...scanText(prepare(file.path, file.text, false)));
  }
  return { scanned, hits };
}

// --------------------------------------------------------------- coverage

function runCoverage() {
  const paths = productionPaths();
  const files = paths.map((path) => ({
    path,
    text: readFileSync(resolve(ROOT, path), 'utf8'),
  }));
  const { scanned, hits } = scanFiles(files);
  const used = new Map();
  const unreviewed = [];
  for (const hit of hits) {
    const entry = ALLOWLIST.find(
      (a) => a.file === hit.path && hit.text.includes(a.line_contains),
    );
    if (entry) {
      used.set(entry, (used.get(entry) ?? 0) + 1);
      continue;
    }
    unreviewed.push(hit);
  }
  for (const entry of ALLOWLIST) {
    const count = used.get(entry) ?? 0;
    if (count !== entry.count) {
      unreviewed.push({
        path: entry.file,
        line: 0,
        kind: 'STALE-ALLOWLIST',
        text: `allowlist entry matches ${count} time(s), expected ${entry.count}: ${entry.line_contains}`,
      });
    }
    if (typeof entry.owner !== 'string' || entry.owner.trim() === '') {
      unreviewed.push({
        path: entry.file,
        line: 0,
        kind: 'ALLOWLIST-OWNER-MISSING',
        text: `allowlist entry has no owner: ${entry.line_contains}`,
      });
    }
  }
  if (scanned === 0) {
    console.error('authority-collapse: scan walked no production files');
    process.exit(1);
  }
  for (const p of unreviewed) {
    console.error(
      `authority-collapse: ${p.path}:${p.line}: UNREVIEWED ${p.kind}: ${p.text}`,
    );
  }
  if (unreviewed.length > 0) {
    process.exit(1);
  }
  console.log(
    `authority-collapse: PASS (${scanned} production file(s) scanned mechanically — no file list, no annotation; ${ALLOWLIST.length} reviewed exception(s); 0 unreviewed error→absence collapses)`,
  );
}

// --------------------------------------------------------------- selftest

function selftest() {
  let failed = 0;
  const expectHits = (name, text, want) => {
    const hits = scanText(prepare('fixture.rs', text, false));
    if ((hits.length > 0) !== want) {
      console.error(
        `authority-collapse selftest: ${name}: expected hit=${want}, got ${hits.length}`,
      );
      failed += 1;
    }
  };

  // --- pattern fixtures: every listed error→absence transformation fires.
  expectHits(
    'collapse on a task read',
    '        let task = handle.get_task(task_id).ok().flatten();',
    true,
  );
  expectHits(
    'unwrap_or_default on memory facts',
    '        let facts = handle.memory_facts().unwrap_or_default();',
    true,
  );
  expectHits('bare Err(_) => None arm', '            Err(_) => None,', true);
  expectHits('bare Err(_) => vec![] arm', '            Err(_) => vec![],', true);
  expectHits('bare Err(_) => Vec::new() arm', '            Err(_) => Vec::new(),', true);
  expectHits('bare Err(_) => false arm', '            Err(_) => false,', true);
  expectHits('bare Err(_) => 0 arm', '            Err(_) => 0,', true);
  expectHits('bare Err(_) => {} arm', '            Err(_) => {},', true);
  expectHits(
    'Err(_) return-default arm is a violation',
    '            Err(_) => return Ok(None);',
    true,
  );
  expectHits(
    'let Ok on an authority read is reviewed, not auto-passed',
    '        let Ok(identity) = handle.identity() else {',
    true,
  );
  expectHits(
    'if let Ok swallowing a page read is a violation',
    '        if let Ok(page) = handle.memory_facts_page(None, 16) {',
    true,
  );
  expectHits(
    'unwrap_or_else fabricating a workspace is a violation',
    '        let ws = handle.identity().map(|i| i.workspace_id).unwrap_or_else(|_| WorkspaceId::new(1));',
    true,
  );
  expectHits(
    'multi-line receiver collapse continuation is a violation',
    [
      '        let project_key = self',
      '            .deps',
      '            .session',
      '            .resolve_workspace_root(handle.id())',
      '            .ok()',
    ].join('\n'),
    true,
  );
  expectHits(
    'typed refusal is NOT a violation',
    '            Err(e) => return Err(ExecError::SemanticRequired(format!("{e}"))),',
    false,
  );
  expectHits(
    'non-authority JSON serialization is NOT a violation',
    '        let spec = serde_json::to_string(spec).unwrap_or_default();',
    false,
  );
  expectHits(
    'a comment mentioning the pattern is NOT a violation',
    '        // handle.get_task(id).ok() must never be used',
    false,
  );
  expectHits(
    'a string mentioning the pattern is NOT a violation',
    '        let note = "Err(_) => None,";',
    false,
  );
  expectHits(
    'an inline cfg(test) body is NOT production',
    [
      '#[cfg(test)]',
      'mod tests {',
      '    fn t() {',
      '        let v = handle.get_task(1).ok();',
      '        let _ = v;',
      '    }',
      '}',
    ].join('\n'),
    false,
  );

  // --- discovery fixtures: NO file list, NO annotation. A planted
  // child-handoff authority using error→absence is discovered by the tree
  // scan alone; a planted verification/routing authority too.
  const plantedChildHandoff = {
    path: 'crates/orchestrator/src/brand_new_child_handoff.rs',
    text: [
      '//! A brand-new child-handoff authority added after the checker shipped.',
      'pub fn child_handoff(handle: &SessionHandle) -> Option<serde_json::Value> {',
      '    let legacy = handle.get_task_ledger().ok().flatten();',
      '    if let Ok(page) = handle.memory_facts_page(None, 16) {',
      '        let _ = page;',
      '    }',
      '    legacy',
      '}',
    ].join('\n'),
  };
  const plantedVerification = {
    path: 'crates/verify/src/brand_new_verification.rs',
    text: [
      'pub fn verified(handle: &SessionHandle) -> bool {',
      '    match handle.ledger_ensure_head() {',
      '        Ok(head) => head.verified,',
      '        Err(_) => false,',
      '    }',
      '}',
    ].join('\n'),
  };
  const plantedRouting = {
    path: 'crates/orchestrator/src/brand_new_routing.rs',
    text: [
      'pub fn route(handle: &SessionHandle) -> Vec<String> {',
      '    let rows = handle.list_sessions(None).unwrap_or_default();',
      '    rows',
      '}',
    ].join('\n'),
  };
  const plainModule = {
    path: 'crates/util/src/helpers.rs',
    text: 'pub fn add(a: u64, b: u64) -> u64 {\n    a + b\n}\n',
  };
  const discovery = scanFiles([plantedChildHandoff, plantedVerification, plantedRouting, plainModule]);
  if (discovery.scanned !== 4) {
    console.error(
      `authority-collapse selftest: planted modules: expected 4 scanned files, got ${discovery.scanned}`,
    );
    failed += 1;
  }
  const kinds = new Set(discovery.hits.map((h) => h.kind));
  if (
    !discovery.hits.some(
      (h) => h.path === plantedChildHandoff.path && h.kind === 'collapse',
    ) ||
    !discovery.hits.some(
      (h) => h.path === plantedChildHandoff.path && h.kind === 'let-ok',
    )
  ) {
    console.error(
      'authority-collapse selftest: the planted child-handoff .ok().flatten() and if-let-Ok must be discovered WITHOUT any annotation',
      discovery.hits,
    );
    failed += 1;
  }
  if (
    !discovery.hits.some(
      (h) => h.path === plantedVerification.path && h.kind === 'bare-err-arm',
    )
  ) {
    console.error(
      'authority-collapse selftest: the planted verification authority swallow must be discovered',
      discovery.hits,
    );
    failed += 1;
  }
  if (!kinds.has('collapse')) {
    console.error('authority-collapse selftest: planted routing collapse must be discovered');
    failed += 1;
  }
  if (discovery.hits.some((h) => h.path === plainModule.path)) {
    console.error('authority-collapse selftest: a plain module must produce no hits');
    failed += 1;
  }

  // --- test-only exclusion: an out-of-line module declared ONLY as
  // `#[cfg(test)]` is invisible; the SAME file with a production
  // declaration is scanned (the declaration analysis decides, not the name).
  const declaringLib = {
    path: 'crates/foo/src/lib.rs',
    text: '#[cfg(test)]\nmod authority_checks;\n',
  };
  const productionLib = {
    path: 'crates/foo/src/lib.rs',
    text: 'mod authority_checks;\n',
  };
  const gatedModule = {
    path: 'crates/foo/src/authority_checks.rs',
    text: 'pub fn check(handle: &SessionHandle) -> bool {\n    handle.get_task(1).is_ok()\n}\nfn bad(handle: &SessionHandle) -> Option<u32> {\n    handle.task_id().ok()\n}\n',
  };
  const gated = scanFiles([declaringLib, gatedModule]);
  if (gated.hits.length !== 0 || gated.scanned !== 1) {
    console.error(
      'authority-collapse selftest: a test-gated out-of-line module must be excluded',
      gated,
    );
    failed += 1;
  }
  const production = scanFiles([productionLib, gatedModule]);
  if (!production.hits.some((h) => h.path === gatedModule.path && h.kind === 'collapse')) {
    console.error(
      'authority-collapse selftest: a non-test declaration makes the module production code',
      production,
    );
    failed += 1;
  }
  const pathGated = scanFiles([
    {
      path: 'crates/foo/src/brand_tests.rs',
      text: 'fn t(handle: &SessionHandle) -> Option<u32> {\n    handle.task_id().ok()\n}\n',
    },
  ]);
  if (pathGated.hits.length !== 0) {
    console.error('authority-collapse selftest: *_tests.rs files must be excluded');
    failed += 1;
  }

  if (failed > 0) process.exit(1);
  console.log(
    'authority-collapse selftest: PASS (error→absence patterns detected; a planted child-handoff/verification/routing authority is discovered with NO filename or annotation; test-gated modules excluded)',
  );
}

const mode = process.argv[2] ?? '--coverage';
if (mode === '--selftest') {
  selftest();
} else if (mode === '--coverage') {
  runCoverage();
} else if (mode === '--selftest-then-coverage') {
  selftest();
  runCoverage();
} else {
  console.error(
    `usage: node ${relative(ROOT, fileURLToPath(import.meta.url))} [--coverage|--selftest]`,
  );
  process.exit(2);
}
