# Findings — Faktor automated test programme review

Source: `Report.pdf` (48 pages, 2026-10-18, `@Aaron`), read in full from
`pdftotext -layout` output. Review HEAD: `ec502901` ("Fixes", 18 Oct 2026).
Current tree: `ec502901cf1d0b0318114f6d88340fb67f99e71e` (361 commits,
2026-10-18). This file records every defect/proof gap the report names, the
report's exact "what it fixed / what still needs proof / test to add", and
the current repository state. Uncommitted work in the shared worktree is
called out separately: it is not part of HEAD and is not release evidence
until committed and verified.

Page references are PDF pages. "Row N" means the At-a-glance table on
pp.3–7. Status vocabulary: **DONE** = code + proof present; **PARTIAL** =
code fix present, report's required proof absent; **MISSING** = neither;
**NEW DEFECT** = fix introduced a defect.

Severity:
- **S0 Critical** — release blocker: data loss/corruption, money/security,
  false completion, or a public-repo integrity breach.
- **S1 High** — wrong behaviour at real scale, or a missing proof on a
  release-critical path.
- **S2 Medium** — proof gap / regression risk; no immediately wrong result.
- **S3 Low** — hygiene, stale metadata, ratchet debt.

## 0. Current snapshot (what the report's counts mean now)

| Fact | Report (pp.1–9) | Current tree |
|---|---|---|
| HEAD | `ec50290` "Fixes", 84 files, ~5,000 code/test lines + 48,000 visual-baseline lines | `ec502901`; same 84-file stat; dumps are in this commit |
| Discovered surfaces | 359 | `node scripts/discover-capabilities.mjs --json` prints `359`; 246 native, 43 CLI, 58 VS Code, 6 provider, 8 tool (the report groups 56 IDE = 55 VS Code + 1 JetBrains) |
| Registry rows | 346 discovered rows, all delegating to 5 generic invariants | `tests/invariant-coverage.json` has `entries` (104 capability rows), `surfaces` (12 rows) and `discovered` (346 rows). All 346 delegate: 229 native, 44 CLI, 58 VS Code + 1 JetBrains, 7 tools + 1 tool lock row, 6 providers. Zero discovered rows have an own behaviour test. |
| Delegation targets | 5 generic invariants | `INV-NATIVE-ROUTE-BEHAVIOR` 229, `INV-CLIENT-ROUTE-CONTRACT` 66 (58 VS Code + 1 JetBrains + 7 tools), `INV-CLI-DOCTOR-EXIT-CODE` 44, `INV-PROVIDER-CLASSIFICATION-TRUTH-TABLE` 6, `INV-SEMANTIC-SEARCH-LOCK-DISCIPLINE` 1 |
| Routes with no row | 17 Revision 8 routes | Exactly 17; `node scripts/check-invariant-coverage.mjs --check` fails with 17 `undiscovered-capability` violations |
| Stale rows | 4 ("drop the 4 stale ones", p.46) | Exactly 4: `cli.help`, `vscode.configuration.faktor.auto-start`, `vscode.configuration.properties`, `vscode.configuration.title` |
| Controls | 200: 40 complete / 148 partial / 12 missing; 175 contract gaps; audit summary stale at 25/160/15 | `docs/specs/edition8/controls.json` still records `counts.by_status = {exists:25, partial:160, missing:15}` and 175 `contract_gap` records; `docs/specs/edition8/backlog.md:7` already says 40/148/12. The hand-written count is stale exactly as reported. |
| Invariants | 158; each has unit + wiring + planted mutation; five carry all 346 delegations | `tests/invariants.toml`; `tests/invariant-coverage.json` |
| Rust tests / crates | 6,880 / 51 | `tests/production-crates.json` has 51 crates |
| Scale probe | `scripts/edition8/scale-probe.mjs`, 1,000 turns, not run by CI | File exists; no `.woodpecker/**` reference (rg finds none) |
| Core dumps | two 89 MB files in history, no credentials found | `qemu_chrome_20261018-133409_41.core` 89,522,176 B and `_43.core` 89,534,464 B are tracked in `ec502901`; `.gitignore:15-32` ignores `core`/`core.*` but not `*.core`. A worktree purge is in flight (see `act-now.md`); HEAD is still dirty. |
| Worktree WIP | not part of HEAD | Uncommitted at review time: settlement policy-row binding, memory range predicate + tests, `scripts/programme/select-tests.mjs`, `tests/programme/**`. Not release evidence until committed. |

The report's headline (p.1): **no surface has its own behaviour proof**; all
346 registry rows delegate to five generic invariants, 17 Revision 8 routes
have no row, and HEAD fixes expose new defects.

---

## 1. At-a-glance findings (pp.3–7)

### AG-01 — Settlement bound to the admitted task (Row 1, p.3)
- **Report fix:** "A prompt starting during the previous turn's settlement
  folded the old goal, spend and completion gate into the new task, and could
  close it before its first model call."
- **Still needs proof:** "Paths that pass no task id (queued prompt, recovery,
  shadow watcher) look up a run row under `tx-session-<id>`. Only a policy row
  is written there, so they fall back to the current task. 18 other lazy
  `task_id()` reads remain in agent and orchestrator code."
- **Test to add:** deterministic interleaving: start prompt N+1 at every await
  point of turn N's settlement; task N+1 must stay byte-identical. Static scan
  banning lazy `task_id()` reads in settlement modules.
- **Current state — PARTIAL.** `settle_run` now carries `task_id:
  Option<TaskId>` (`crates/orchestrator/src/task_executor/settlement.rs:36-56`);
  `InSession` call sites pass it. But `settle_resolved_verifications` sets
  `task_id: None` (`settlement.rs:243-247`) and `recovery.rs:570` does the
  same; both fall back to `settlement_task_id` (`settlement.rs:8-23`), which
  ends in `handle.task_id()`. `settle_orchestrated` still calls
  `handle.task_id()?` (`settlement.rs:911`); `settle_resolved_verifications`
  does too (`settlement.rs:283`). The report's 18 lazy reads are still
  visible in `crates/agent/src/runtime/**` and
  `crates/orchestrator/src/**` (for example `settlement.rs:20,283,911`,
  `turn/drive.rs:724,1499`, `turn/state.rs:1385,2513`,
  `compaction.rs:981,1145`, `retrieval.rs:948,987`,
  `verification_attribution.rs:919`, `outcome_ledger.rs:2558,2606`,
  `orchestrator/runtime.rs:977`, `runtime_helpers.rs:32`). No static ban, no
  interleaving harness.
- **Severity: S0.** Wrong task completion / spend attribution is the
  product's core promise.

### AG-02 — Tool activation by point lookup (Row 2, p.3)
- **Report fix:** "A `source_market` activation silently switched off once
  3,200 newer internal facts existed."
- **Still needs proof:** "The `BoundExhausted` state is now dead in
  production code."
- **Test to add:** activation survives 1,000,000 churn rows; a corrupt value
  reads as failed, never as inactive.
- **Current state — PARTIAL.** `crates/agent/src/runtime/retrieval.rs:1761-1807`
  now uses `memory_fact_get(TOOL_ACTIVATION_FACT_KIND,
  TOOL_ACTIVATION_FACT_KEY)`; a corrupt value returns `ReadFailed`.
  `ToolActivationScan::BoundExhausted` still exists at
  `crates/agent/src/runtime/tool_loop.rs:435-447` and is referenced by the
  test-only diagnostics module (`activation_scan_diagnostics_tests.rs:13`),
  so it is dead production code exactly as reported. The existing regression
  test covers unrelated churn but not 1,000,000 rows.
- **Severity: S1.**

### AG-03 — Child control rows filtered by kind (Row 3, p.4)
- **Report fix:** "Long sessions failed turns with 'refusing a partial control
  read' past 5,000 facts."
- **Still needs proof:** "A hostile row space still stops at 25 pages."
- **Test to add:** 1,000,000 unrelated facts plus 5,001 control rows: typed
  refusal, never a truncated queue.
- **Current state — PARTIAL.** SQL is now kind-filtered:
  `crates/session/src/child.rs:61-66` (`ROW_KIND_CONTROL =
  "orchestrator_ctl"`, `CONTROL_PAGE = 200`, `MAX_CONTROL_PAGES = 25`) and
  `child.rs:606-643` uses `memory_facts_by_kind_page(ROW_KIND_CONTROL, ...)`.
  The regression test (`child.rs:767+`) seeds 5,200 `taskexec_run` rows plus
  one control; it does not exceed the 5,000-row control window or seed
  1,000,000 unrelated rows.
- **Severity: S1.**

### AG-04 — Incremental ledger verification (Row 4, p.4)
- **Report fix:** "Every session open re-decoded the whole ledger, quadratic
  over a turn loop."
- **Still needs proof:** "Corruption inside the already-verified prefix is
  caught only after a restart."
- **Test to add:** corrupt a verified row; a scheduled full sweep must fail
  loudly.
- **Current state — PARTIAL.** `SessionHandle::ledger_verify_open` delegates
  to `ledger_verify_after(0)` and the process caches the verified cursor
  (`crates/session/src/ledger/mod.rs:3712-3760`;
  `crates/session/src/manager.rs:1263-1315`). The test
  `incremental_verify_catches_new_corruption_after_the_verified_prefix`
  (`crates/session/src/ledger/tests.rs:2120-2165`) corrupts a row *after* the
  verified prefix. There is no scheduled full sweep and no corrupt-the-prefix
  test.
- **Severity: S2.**

### AG-05 — Memory paging filtered in SQL (Row 5, p.4) — **new defect**
- **Report fix:** "Context preparation paged through every internal fact on
  each turn."
- **Still needs proof / new defect:** "LIKE is case-insensitive, treats `_` as
  a wildcard and cannot use the index. On the same schema: 49 ms per page at
  200,000 churn rows, against 0.02 ms with `kind >= 'memory_v2:' AND kind <
  'memory_v2;'`."
- **Test to add:** query-plan gate naming the `(session_id, kind, key)` index;
  200,000-row timing test; kinds `MEMORY_V2:x` and `memoryXv2:y` must not
  match.
- **Current state — NEW DEFECT at HEAD; WIP fix in worktree.** Both pagers
  still interpolate a `LIKE` at HEAD:
  `crates/store/src/ledger.rs:1655` in `memory_facts_page_filtered`
  (`AND kind LIKE '{prefix}%'`) and `crates/store/src/longtask.rs:404` in
  `workspace_memory_facts_page_filtered`. The count query also uses it:
  `longtask.rs:506` `WHERE session_id = ?1 AND kind LIKE 'memory_v2:%'`.
  `memory_facts_v2_page`/`workspace_memory_facts_v2_page` pass the literal
  prefix `memory_v2:`. The table's `UNIQUE (session_id, kind, key)`
  (`crates/store/src/schema.rs:107-115`) creates the
  `sqlite_autoindex_memory_fact_1` index that the range can use; the extra
  `idx_memory_fact_kind_session (kind, session_id)` and
  `idx_memory_fact_session_key (session_id, key)` are not the exact
  `(session_id, kind, key)` path. Uncommitted worktree changes add
  `kind_prefix_end`, replace all three predicates with the half-open range,
  and add literal-prefix/200k-plan tests (`ledger_tests.rs`); they are not
  committed and the plan test currently names the autoindex. No release
  evidence yet.
- **Severity: S1** (correctness: case-insensitive/underscore false positives;
  performance: 49 ms/page at 200k).

### AG-06 — Workspace cache capped at 64 roots (Row 6, p.4)
- **Report fix:** "The probe accumulated over 500 file-watcher threads."
- **Still needs proof:** "Eviction drops an arbitrary entry, possibly the hot
  durable root. The new test forces a 1 ms deadline, so it never runs a real
  watcher."
- **Test to add:** 1,000 shadowed turns per OS with real watchers: thread and
  handle counts stay flat; an in-use durable root is never evicted.
- **Current state — PARTIAL.** `MAX_CACHED_ROOTS: usize = 64` and eviction of
  the first non-current key exist (`crates/fs/src/workspace_service.rs:28,
  353-374`). The regression test `cached_workspace_roots_are_hard_bounded`
  uses `with_watch_registration_deadline(Duration::from_millis(1))`
  (`crates/fs/src/lib.rs:3642-3663`), exactly the report's "never runs a real
  watcher". No per-OS thread/handle census, no in-use-root assertion.
- **Severity: S1.**

### AG-07 — Routing reads a 64-call tail (Row 7, p.4)
- **Report fix:** "Each turn re-read every provider call in the session."
- **Still needs proof:** "None found."
- **Test to add:** on recorded sessions, the 64-call tail gives the same
  routing decision as full history.
- **Current state — PARTIAL.** `ROUTING_PREFIX_HISTORY: u64 = 64` and
  `provider_call_prefix_rows_limited(session, Some(64))` are at
  `crates/session/src/manager.rs:638-642`; the store API is
  `crates/store/src/billing.rs:1197`. The only test,
  `provider_prefix_history_tail_is_bounded_and_oldest_first`
  (`crates/store/src/billing_tests.rs:470-510`), asserts bounded/oldest-first,
  not decision equality against full history.
- **Severity: S2.**

### AG-08 — History paging (`beforeMs`, `beforeId`, `limit`) and new index
### (Row 8, p.5)
- **Report fix:** "History with 2,000 conversations loaded the whole table."
- **Still needs proof:** "The cursor sorts on `updated_ms`, which changes. A
  conversation updated mid-walk jumps to page one and later pages skip it."
- **Test to add:** update conversations during a page walk; define the rule
  (snapshot or re-surface) and assert it.
- **Current state — PARTIAL.** Cursor route is
  `crates/server/src/native/session.rs:1488-1560`;
  `Store::list_sessions_page` orders `updated_ms DESC, id DESC`
  (`crates/store/src/sessions.rs:561-603`); migration v39 adds
  `idx_session_updated_id ON session(updated_ms DESC, id DESC)`
  (`crates/store/src/schema.rs:1200-1207`). No mid-walk update test, no
  documented snapshot/re-surface rule, no query-plan test.
- **Severity: S1.**

### AG-09 — Transcript search reads answer parts (Row 9, p.5)
- **Report fix:** "Model answers were invisible to Find."
- **Still needs proof:** "Search matches raw JSON, so `text`, `parts` or a
  quote character match every row. `lower()` folds ASCII only, so 'É' never
  finds 'é'."
- **Test to add:** results must equal a reference search over the visible
  decoded text; Unicode case-folding corpus.
- **Current state — PARTIAL.** `Store::search_messages` searches
  `instr(lower(m.data), lower(?2))` plus `part.data`
  (`crates/store/src/messages.rs:433-470`). It stores/returns raw message
  rows; `native_transcript_search` builds snippets from raw `text`/part JSON
  (`crates/server/src/native/foundation_ext.rs:348-430`). No decoded-text
  oracle, no Unicode case-folding test, no 100,000-row bound test.
- **Severity: S1.**

### AG-10 — Proof totals cover every row (Row 10, p.5)
- **Report fix:** "A 10,000-criterion record reported page-local counts."
- **Still needs proof:** "None found."
- **Test to add:** totals at 0, 1, 499, 500, 501 and 10,000 rows equal the
  durable counts.
- **Current state — PARTIAL.** `proof.rs` counts every record criterion and
  check while paging only `items` (criteria: `crates/server/src/native/proof.rs:
  810-870`; checks: `proof.rs:873-930`), with `truncated` set from
  `record.criteria.len() > items.len()`. The scale probe asserts 10,000
  totals (`scripts/edition8/scale-probe.mjs:963-987`), but the report's
  boundary set 0/1/499/500/501 is not a unit test, and a broken chain
  rendering as verified is untested.
- **Severity: S2.**

### AG-11 — Tiny-history compaction (Row 11, p.5)
- **Report fix:** "Compacting a one-token history crashed the daemon
  (`clamp(1, 0)` panic)."
- **Still needs proof:** "Other routes may hide the same panic class."
- **Test to add:** fuzz every route's request body with boundary values under
  a panic hook that fails the run.
- **Current state — PARTIAL.** `compaction.rs` now refuses histories below
  `MIN_COMPACTABLE_HISTORY_TOKENS = 128` (`crates/agent/src/runtime/
  compaction.rs:480-493`), and the adversarial size matrix landed: token
  sizes 0/2/3/127 refuse typed with zero compaction-model calls, 128
  journals CompactRejected, the wire layer pins 1-token as unconstructible,
  and the durable layer rejects `before`<=2/-growth as Malformed. A
  repo-wide scan finds no literal `clamp(1, 0)`, but there is no
  ROUTE-wide boundary fuzz + panic hook. The report's class fix is not
  yet fully enforced.
- **Severity: S1.**

### AG-12 — Budget caps accepted on plain prompts (Row 12, p.5)
- **Report fix:** "Caps were refused on this path after a 1-micro cap once let
  a paid call through."
- **Still needs proof:** "No new test proves a cap of '1' stops the first paid
  call here. A cap of 0 means no cap."
- **Test to add:** `max_cost_micro: "1"` and `max_tokens: 1` give zero
  provider calls and a typed refusal; the zero-cap rule is shown and tested.
- **Current state — PARTIAL.** Caps are threaded into the prompt/task request
  (`crates/server/src/native/prompt.rs:68-70,213-214`;
  `crates/server/src/native/task.rs:710-744`) and the executor seeds the
  durable task row. `task.rs:731` documents "A zero cap stays absent (the
  historical 'no override' shape)". There is a child-budget zero-refusal test
  (`crates/server/src/api/router_task_tests.rs:1030-1047`) but no
  plain-prompt 1-micro/1-token test and no zero-cap plain-prompt test.
- **Severity: S1.**

### AG-13 — Task and run state tags (Row 13, p.6)
- **Report fix:** "A finished turn showed 'running' forever; verifying,
  blocked and waiting showed as done."
- **Still needs proof:** "The two IDEs render the tags separately."
- **Test to add:** every task state × session state pair maps to one tag,
  rendered the same in both IDEs.
- **Current state — PARTIAL.** `run_state_tag_for` explicitly maps
  Verifying/Blocked/Waiting before the stale Running/Pending→Done fallback
  (`crates/server/src/native/task.rs:21-76`); `task_view_state_tag` handles the
  session view (`crates/server/src/native/session.rs:249-270`). Unit tests
  cover individual pairs (`task.rs:1200-1250`), not the exhaustive pair table
  served by the daemon and rendered by both hosts.
- **Severity: S1.**

### AG-14 — VS Code tool outcome merge (Row 14, p.6)
- **Report fix:** "A denied call showed 'read_file done' beside an '(unnamed
  tool) exit 1' bubble."
- **Still needs proof:** "Merging runs after the transcript is cut to its
  newest window, so a call just outside it leaves an orphan bubble. A denial
  with text but no exit code still shows 'done'."
- **Test to add:** every tool × outcome (allowed, denied, cancelled, timed
  out, unknown effect), with and without exit code, never shows a pass tone;
  window-edge case.
- **Current state — PARTIAL.** `apps/vscode/scripts/transcript-deny-check.mjs`
  asserts a denied result-only message merges into the original call with
  `exitCode = 1`. The merge is in `apps/vscode/media/chat.js`; no window-edge
  or no-exit-code denial test.
- **Severity: S1.**

### AG-15 — VS Code webview handshake and backpressure (Row 15, p.6)
- **Report fix:** "Panels stayed empty when the ready message or patches were
  lost."
- **Still needs proof:** "Only the fixed ordering is covered."
- **Test to add:** message before listener, hide and reveal mid-refresh,
  reload, 100 rapid reveals: each ends showing the complete served state.
- **Current state — PARTIAL.** `apps/vscode/src/webview.ts:43-46` batches one
  pending snapshot; the host/panel handshake is in
  `apps/vscode/src/edition8/panel.ts`. Tests cover the fixed ordering only;
  adversarial lifecycle is absent.
- **Severity: S1.**

### AG-16 — JetBrains scrollbar button names (Row 16, p.6)
- **Report fix:** "The accessibility audit failed on unnamed scrollbar
  buttons."
- **Still needs proof:** "The fix targets the Metal look-and-feel of the
  headless harness, not the real IDE's."
- **Test to add:** run the audit inside a real IDE with its own look-and-feel.
- **Current state — PARTIAL.** `installScrollButtonAccessibility` only names
  `javax.swing.plaf.metal.MetalScrollButton` and is installed by both screen
  registries (`apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/
  UiWork.kt:804-818`; `Edition5ScreenRegistry.kt:63-66`;
  `Edition8Panels.kt:89-92`). No real-IDE look-and-feel test.
- **Severity: S2.**

### AG-17 — Migration tests use fixed versions (Row 17, p.6)
- **Report fix:** "Tests written as `SCHEMA_HEAD − N` silently skipped a
  migration when v39 landed."
- **Still needs proof:** "Other arithmetic offsets may remain."
- **Test to add:** for every version from 0 to head: migrate, then compare the
  whole schema to a fresh head database.
- **Current state — PARTIAL.** Some offsets were replaced by named constants
  (`crates/store/src/migration/ladder_tests.rs:1279` uses `prev_head = 34`;
  `schema_head_guard_tests.rs:179` uses `TOMBSTONE_PREDECESSOR = 37`). Other
  `SCHEMA_HEAD - 1` uses remain (`pruning_tests.rs:208,239,242`;
  `schema_head_guard_tests.rs:269,293`). The per-version matrix
  (`hostile_corpus_tests.rs:385-430`) migrates and writes a row but does not
  compare the full schema to a fresh head DB.
- **Severity: S2.**

### AG-18 — Skills listing reports totals (Row 18, p.7)
- **Report fix:** "Workspaces with over 200 skills silently showed 200."
- **Still needs proof:** "There is no cursor: skill 501 onward is
  unreachable."
- **Test to add:** all 600 discovered skills are reachable.
- **Current state — PARTIAL.** `native_skills` now reports
  `discoveredTotal`/`hasMore` and caps at 500
  (`crates/server/src/native/foundation_ext.rs:1460-1555`); no cursor, and the
  installed list also `.take(500)` (`foundation_ext.rs:1492-1515`). No
  600-skill reachability test.
- **Severity: S2.**

### AG-19 — Two core dumps added (Row 19, p.7) — **not a fix**
- **Report:** "Two committed Chromium core dumps (`qemu_chrome_20261018-
  133409_41.core` and `_43.core`, 89 MB each) in history. A pattern scan
  found no credentials in them, but every clone of the public repo downloads
  them."
- **Still needs proof:** ".gitignore covers `core.*` but not `*.core`; the
  hygiene scan checks a fixed list of extensions."
- **Test to add:** sniff file content (ELF core, minidump, Mach-O) and cap
  the size of any blob not on an allowlist.
- **Current state — MISSING.** Both files are tracked in `ec502901`; blobs
  `c648467fd8783f070a9b01afa0fba1be0f0be126` and
  `502deb5d80def31950ffbd6d53a6b88da1af6a90`; paths at repo root. `.gitignore`
  has `core`, `core.*` and source negations, but no `*.core`. No content
  sniff gate.
- **Severity: S1.** Public clone size and repo hygiene; no secrets per report.

### AG-20 — Another file-size exemption (`sessions.rs`, "split deferred")
### (Row 20, p.7) — **not a fix**
- **Report:** "The exemption list only grows."
- **Test to add:** ratchet: exemptions may only shrink.
- **Current state — MISSING.** `tests/static-authority/src/lib.rs:7762-7778`
  has `SOURCE_SIZE_CEILING_LINES = 4000` and the shrink-only grandfather
  list including `("crates/store/src/sessions.rs", 4035)` with the comment
  "Rev8 History paging added the cursor/listing helpers and tests; split
  deferred". The gate rejects growth and requires removal once the file fits,
  but the list can in principle accept new entries in review; no test asserts
  the list length is non-increasing across commits.
- **Severity: S3.**

---

## 2. Registry, coverage and surface findings (pp.7–11, 39–40)

### REG-01 — Zero surfaces have their own behaviour row (p.1, pp.7–10) — S0
The report: "no surface yet has its own behaviour proof in the coverage
registry. All 346 registry rows delegate to five generic invariants."
`tests/invariant-coverage.json` confirms 346 `discovered` rows; 229 native
rows delegate to `INV-NATIVE-ROUTE-BEHAVIOR`, 66 client rows (58 VS Code +
1 JetBrains + 7 tools) to `INV-CLIENT-ROUTE-CONTRACT`, 44 CLI rows to
`INV-CLI-DOCTOR-EXIT-CODE`, 6 provider rows to
`INV-PROVIDER-CLASSIFICATION-TRUTH-TABLE`, 1 tool row to
`INV-SEMANTIC-SEARCH-LOCK-DISCIPLINE`. Rule 8 (p.11): "No delegated coverage.
A surface counts as covered only by a test of its own behaviour." The
checker's `undiscovered-capability` rule is at
`scripts/check-invariant-coverage.mjs:288`; delegation validation is at
`:203-245`. **Zero of the 346 rows name an own test.**

### REG-02 — 17 Revision 8 routes have no row (pp.1,7,23,29–35) — S1
`node scripts/check-invariant-coverage.mjs --check` currently fails with
exactly 17 violations. The routes and handlers are:

| # | Route (method + path) | Handler | Registration |
|---|---|---|---|
| 1 | `GET /native/catalog/probe` | `foundation::native_catalog_probe` | `crates/server/src/api/lifecycle.rs:1031` |
| 2 | `GET /native/presets/{id}/roles` | `r8::native_preset_roles_get` | `lifecycle.rs:1022` |
| 3 | `POST /native/presets/{id}/roles` | `r8::native_preset_roles_set` | `lifecycle.rs:1022` |
| 4 | `GET /native/presets/{id}/fallback` | `r8::native_preset_fallback_get` | `lifecycle.rs:1026` |
| 5 | `POST /native/presets/{id}/fallback` | `r8::native_preset_fallback_set` | `lifecycle.rs:1026` |
| 6 | `GET /native/mcp/servers/{id}/auth` | `foundation_ext::native_mcp_server_auth_get` | `lifecycle.rs:1180` |
| 7 | `POST /native/mcp/servers/{id}/auth` | `foundation_ext::native_mcp_server_auth` | `lifecycle.rs:1181` |
| 8 | `GET /native/session/{id}/review/comments` | `foundation_ext::native_review_comments_list` | `lifecycle.rs:1101` |
| 9 | `POST /native/session/{id}/review/comments` | `foundation_ext::native_review_comment_create` | `lifecycle.rs:1101` |
| 10 | `PATCH /native/session/{id}/review/comments/{comment}` | `foundation_ext::native_review_comment_update` | `lifecycle.rs:1106` |
| 11 | `DELETE /native/session/{id}/review/comments/{comment}` | `foundation_ext::native_review_comment_delete` | `lifecycle.rs:1106` |
| 12 | `POST /native/session/{id}/changes/accept` | `foundation_ext::native_change_accept` | `lifecycle.rs:1112` |
| 13 | `POST /native/session/{id}/checkpoints/{sequence}/restore/preview` | `foundation_ext::native_checkpoint_restore_preview` | `lifecycle.rs:1097` |
| 14 | `DELETE /native/session/{id}` | `foundation_ext::native_session_delete` | `lifecycle.rs:1120` |
| 15 | `POST /native/session/{id}/export` | `foundation_ext::native_session_export` | `lifecycle.rs:1124` |
| 16 | `GET /native/session/{id}/settings` | `foundation_ext::native_session_settings_get` | `lifecycle.rs:1126` |
| 17 | `PUT /native/session/{id}/settings` | `foundation_ext::native_session_settings_put` | `lifecycle.rs:1126` |

The 17 are named by tests in
`crates/server/src/native/foundation_rev8_contracts_tests.rs` (presets,
catalog, review, settings, accept, export, restore preview), but naming a
route in a test is not a registry row; the checker's discovery equality does
not accept test names.

### REG-03 — Four stale discovered rows (p.46 build order) — S2
`cli.help`, `vscode.configuration.faktor.auto-start`,
`vscode.configuration.properties`, `vscode.configuration.title` no longer
match a discovered surface. They must be deleted, not left as permanent
noise.

### REG-04 — 35 routes are named by no test; credentials are the worst (pp.7,8,33) — S1
The report: "35 routes are named by no test, including credential store,
rotate and delete." The credential routes are
`GET/POST /native/providers/credentials` and
`POST /native/providers/credentials/rotate` and
`DELETE /native/providers/credentials/{handle}`
(`crates/server/src/api/lifecycle.rs:740-753`;
handlers at `crates/server/src/native/foundation.rs:567-620`). No test file
names those paths (`rg` finds only production code). The report's rule
(p.32): "a planted secret must never appear in any response, log, export,
event frame or crash dump."

### REG-05 — 19 CLI subcommands run by no test (pp.8,35,36) — S1
The report: "No test runs 8 `updater`, 10 `enterprise` and 1 `sandbox`
subcommands by name." Current CLI discovery shows 43 commands; the registry's
CLI rows all delegate to `INV-CLI-DOCTOR-EXIT-CODE`, which only proves the
doctor exit-code path. A CLI template (p.13) exists in the programme but not
in the registry.

### REG-06 — Control audit is hand-written and stale (pp.8,10) — S1
`docs/specs/edition8/controls.json` still carries
`counts.by_status = {exists:25, partial:160, missing:15}` (line 39), while
`docs/specs/edition8/backlog.md:7` says 40/148/12. The report adds: 175
controls carry a contract gap; two "missing" controls now have routes. Rule 9
(p.11): "Status is derived, never typed. Control status, coverage counts and
baselines are generated from code and test results on every run."

### REG-07 — Windows JetBrains visual records are missing (pp.9,40) — S2
`apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json`
carries only `linux` and `macos`. The Edition 5 and Edition 8 baseline files
have `windows`, but the report's row is about the primary visual contract
file. `scripts/windows-visual-baseline.ps1:379` explicitly refuses a missing
Windows baseline, so the gate is correct but the artifact is absent. The
report also says "Linux baselines were written by the pipeline" — the PR lane
runs `node scripts/edition8/run.mjs --bootstrap-baselines
--allow-unreviewed-platform` (`.woodpecker/untrusted/pr.yaml:537,550`).

### REG-08 — Screen readers are name/role checks only (p.9) — S2
The workflow layer has 5 cells per IDE (`tests/edition8/workflows.mjs`), but
the screen-reader cell checks names and roles. The report records real NVDA,
VoiceOver and Orca runs as "not certified". Required evidence: scripted
screen-reader runs capturing spoken output per workflow.

### REG-09 — `faktor-foundation` is the thinnest-tested crate (p.9) — S2
Report: 13,400 lines and 62 tests, the thinnest ratio in the workspace. It
owns instructions, memory, skills, hooks, MCP, presets, permissions and
context. The current registry gives it no dedicated suite; the control
exercise passes 200/200 without proving each behaviour.

### REG-10 — 158 invariants remain the strongest layer, but five carry all
delegations (p.9) — S2
`tests/invariants.toml` has 158 entries, each with unit, wiring and planted
mutation. That strength is real, but it proves only the shared property. The
programme must keep these rows and add the surface rows beside them.

### REG-11 — In-flight programme taxonomy diverges from the report's six
### layers (p.2; worktree only) — S2
The uncommitted `tests/programme/model.mjs` defines six layers as
`structural`, `unit`, `property`, `fault`, `integration`, `live` and calls
them "the report's execution model". The report's page-2 diagram defines a
different six: **Unit and property**, **State model and crash**,
**Contracts**, **IDE and screen readers**, **Agent evals**, **Hardware**.
The WIP has no first-class agent-eval or hardware layer (folded into
`live`), and adds `structural`, which the report treats as a gate, not a
layer. `tests/programme/selector.mjs` maps changes to crates/suites, not to
the per-surface row (`owns`/`tests`) required by rule 8. Reconcile the WIP
with `programme.md` §3/§5 before landing; do not let the code's comment
rename the report's model.

---

## 3. Shared-harness findings (pp.16–17) — all S1/S2 proof gaps

The report's "Today / Add" table names 18 harness pieces. Current state:

| # | Harness piece | Report "Add" | Current state |
|---|---|---|---|
| HAR-01 | Surface stub generator | one failing stub per surface × template case; a still-failing stub blocks release | DONE. `scripts/harness/surface-stubs.mjs` (`faktor.harness.surface-stub/v1`): regenerates from `discover-capabilities.mjs` each run (428 live surfaces × 5 template cases = 2140 pairs; 1845 stubbed + 295 exclusions each carrying a required reason: `ide-host-required`, `wire-format-not-expressible`, `routing-config-not-endpoint`). The gate fails on a missing pair, an orphan, a duplicate, a forged class, an exclusion without reason, or a stale pinned file; the selftest plants corruptions that must fail. Gate + selftest run in dogfood §7c. |
| HAR-02 | Scripted model responses | record/replay real provider streams with secrets scrubbed | DONE. `scripts/harness/record-replay.mjs` (`faktor.harness.recording/v1`): scrub BY VALUE before any byte hits disk (auth/api-key headers, `Bearer`/`sk-` material, JSON key fields, env `KEY=VALUE` values ≥8 chars, URL userinfo/query secrets, cookies, home paths) with recorded digests instead of values; replay is byte-exact (base64 frames, one `write()` per recorded frame, scaled deltas). Selftest plants secrets across every channel incl. decoded frames (zero survivors byte-scanned), a scanner-sensitivity control proves the scan non-vacuous, and the parsed-event sequence equals the original. Gated in dogfood §7c. |
| HAR-03 | Transport chaos | chaos proxy: latency, drops, truncated/malformed SSE, split UTF-8, 429/5xx + Retry-After | DONE. `tests/chaos` (`faktor-tests-chaos`): a pure-tokio deterministic proxy (seeded, no randomness) drives the REAL daemon against an in-repo mock SSE provider through fault classes `kill_mid_sse`, `truncate`, `reorder`, `delay_burst`, `rst_after` (persistent + one-shot) and `gzip_truncate` (hand-built DEFLATE); each class pins its contract (typed non-retryable Malformed vs retryable Network, retry budget exactly consumed or recovered), exactly-once attempt/ledger rows, `/native/ready` after the fault, zero orphan processes, bounded wall time. One registered `[live]` test fires a real kill-mid-stream against api.deepseek.com with exactly one paid completion. 24 green + 1 registered live. |
| HAR-04 | Crash injection | failpoint at every durable write (fsync, rename, commit) | PARTIAL → fsync/rename COMPLETE AND DRIFT-ENFORCED: `crates/fs/src/fault.rs` enumerates all 10 durability-boundary trip points (per-writer temp fsync ×3, adopt fsync, parent fsync ×every call site, rename — every non-test rename/hard-link routed through `durable_rename_raw`/`durable_hard_link_raw`, rooted publish renamed+fsynced) with one-shot thread-local `(point, ordinal)` arming; two source-scan drift tests FAIL THE BUILD on a raw rename/link outside the wrappers or an unregistered/missing trip literal. SQLite commit boundaries remain per-subsystem (CAS seam, orchestrator settlement seams, store `CrashSeam`, session crash-seam enumeration leg in `tests/chaos`); a uniform failpoint over every SQLite commit statement is the remaining delta. |
| HAR-05 | Store faults | `SQLITE_BUSY`, disk full, read-only FS, corrupt page under every store op | DONE. `crates/store/src/store_fault_ops.rs` (260 op drivers enumerated by the same source-scan discipline as HAR-10; drift test fails in BOTH directions) + `store_fault_matrix_tests.rs`; seam `FaultKind{DiskFull(13),ReadOnly(8),Busy(5)}`/`FaultSeam` compiled out of production, injected at the single writer-admission funnel and the read checkout. Matrix: REAL `SQLITE_BUSY` via second connection `BEGIN IMMEDIATE` (deterministic +150 ms block, bounded join, 3 documented non-blocking exemptions, committed baseline re-verified); ENOSPC/READONLY shims returning the exact SQLite codes 13/8 with seam-fired assertions killing vacuous passes; real read-only chmod (destructive purge covered by 3 other classes; fresh open on ro copy refuses typed; chmod-back reopen proves recovery-on-next-open); real corrupt-page byte-flip in a truncate-checkpointed copy run under ALL 260 ops with a strict control (the flip must be effective — serving the row at all fails) and full-integrity refusal; cross-class DiskFull→RO→Busy sequence survives + reopen recovers. All bounded deadlines, typed-or-clean. |
| HAR-06 | Interleaving control | pause at named await points, run every ordering of two drives | DONE. `tests/chaos/src/interleaving.rs`: exhaustive order-preserving interleaving enumerator (binomial-bounded, ≤512 schedules, deterministic schedule index for replay) driving double-settle, refund-vs-settle, reserve-vs-settle, steering-during-settle, queue push/pop and session-switch-during-append with independent oracles (hand-arithmetic money, journal bleed, exactly-once). Intra-transaction yields (unstepable from outside production crates) are covered by the crash-seam enumeration leg: seam actually fired, world OLD-or-NEW, row↔event coupling mid-crash, replay reaches the never-crashed reference world. Harness self-checks explosion refusal and schedule order. |
| HAR-07 | Property testing | proptest with shrinking and stored seeds | DONE. `tests/router-properties` (7 properties converted, case counts preserved 1:1: 4000/2000/400×4/200; 2 fixed adversarial tails kept as plain tests) and `tests/economy` (fold property converted; the original fixed 60-seed corpus kept verbatim as its own test). Harness proofs in BOTH crates: shrinking drives a failing property to the exact minimal input, `FileFailurePersistence` persists the failing seed and replays it, the same RNG seed reproduces the identical case stream (diverges for another seed). `proptest = "1"` dev-dep in the crates' own manifests only; `proptest-regressions/` persists on first real failure. |
| HAR-08 | Concurrency checking | loom/shuttle for single-writer store, writer queues, admission leases, scheduler budgets | DONE VIA DETERMINISTIC INTERLEAVING (no loom/shuttle dep — deliberate: production crates stay hook-free). The HAR-06 enumerator covers the single-writer/settlement/queue/lease shapes as full order enumeration with per-schedule invariants; in-code loom models remain a documented future option behind `#[cfg(loom)]`. |
| HAR-09 | State-model testing | reference session state machine; random command sequences replay to the same state | DONE. `crates/session/tests/session_modelcheck.rs`: an INDEPENDENT reference session model (the oracle reuses no production code) driven side-by-side with the real runtime by seeded, replayable command sequences; every step compared on observable state AND on journal replay; crash-at-boundary reconstruction validates the OLD-or-NEW window, money recovery and the modeled landing state. Strict per-step seam scoping (an unfired arm is re-armed to a never-firing sentinel; seam-caused typed writer errors classified by the exact `[fault-seam]` marker on three surfaces; non-seam panics are loud divergences). The campaign asserts >0 seam hits (vacuous = FAIL; 73 hits/run on the long tier). Fast tier (5 tests) green in normal runs; long campaign `[fault]`-registered. The harness's second minimized repro also corrected the MODEL for a real contract: after crash recovery, `cost_refund` refuses every dispatch-marker-carrying row as `RefundDispatched` (marker survives both recovery and the settle UPDATE) — pinned in the model. |
| HAR-10 | Query plans | a plan test for every SQL statement; missing plan fails build | DONE. `crates/store/query_plan_inventory.json` + `query_plan_inventory_tests.rs`: 577 statements (source scanner with literal/comment-masked extraction, migration batches split, cfg(test) stripped; 3 representatives for runtime-assembled statements) × 633 EXPLAIN-query-plan cases; 176 schema-ladder occurrences replayed; 54 dynamic assembly sites mapped (2 reasoned exclusions); 28 scan-blessings each with a written reason. Drift checks proven live by probe (inserted statement failed the gate, reverted, green again): unlisted/stale/ladder-order/format-flag/class violations all fail; a missing plan fails (query/non-INSERT mutate must be plan-bearing; exact plan lines vs golden); any remaining SCAN of an indexed table requires `allow_scan`; typed-ledger range reads must use their named indexes. Recorder: `record_query_plan_inventory` (registered manual). |
| HAR-11 | Resource census | thread/file-handle/child-process/memory counts before/after each suite | DONE. `scripts/resource-census.mjs`: wraps any suite command; samples threads/fds/sqlite-connections/children/RSS before+after (ps/lsof on macOS, /proc on Linux, graceful skip where unavailable); two-layer orphan detection (detached process group + descendant census + post-exit pgid scan); justified thresholds (orphans=0, threads≤512, handles≤4096, RSS≤6 GiB, root growth≤2 GiB, all flag-overridable); JSON reports under `target/resource-census/`. Selftest fires all four planted leaks and passes a clean child; census-clean evidence recorded for `cargo test -p faktor-session` and the dogfood selftest. Gated in dogfood §7c. |
| HAR-12 | Panic hook | route fuzzing and long runs turn any panic into a failure with input | DONE. `tests/quality`: `panic_hook.rs` (thread-scoped payload+forced backtrace+attributed input, budgeted minimizer, content-addressed regression saves) and `route_fuzz_tests.rs` (seeded SplitMix64 property fuzz over route/mode/permission decision surfaces; edge corpus: empty, oversized, bidi, mixed-case, unicode/IDN, hostile pins, floor extremes, u64::MAX tokens). Any panic ⇒ minimized input printed + saved to `tests/quality/regressions/` + test failure. Fast tier + corpus replay + harness self-check (planted panic caught/minimized/saved) green; ~110k seeded iterations with zero router/permission panics; 50k `[fault]` soak registered manual. |
| HAR-13 | Scale probe | DONE. Nightly `scale` lane runs `node scripts/edition8/scale-probe.mjs --turns 1000` (`.woodpecker/trusted/nightly.yaml`) with stored curves and absolute asserts; the run also compares against the committed per-platform `scripts/edition8/scale-baseline.json`, and `check-superlinearity.mjs --selftest` (injected violation shapes must fire) gates the PR and nightly lanes. | S1 |
| HAR-14 | Fixture repositories | polyglot monorepo, 1M-line legacy, multi-repo, broken builds, STM32/ESP-IDF/Zephyr | PARTIAL → SCOPE LANDED. `tests/eval` (`faktor-tests-eval`): seeded (SplitMix64, byte-identical cross-platform) generators for a 19-file polyglot monorepo (Rust/TS/Python/Go/Kotlin with cross-language symbol citations; retrieval ground truth in manifest.json; an AND-query over the crossref identifier returns exactly the ground truth across all five languages) and an exactly-1,000,000-line legacy corpus (5,000 modules; manifest total asserted); measured regression budgets committed (`fixtures/budgets.json`, fail only above max(3×baseline, CI floor): 1M generate 912 ms→60 s bound, 1M index 38.5 s→120 s bound); watcher/entry-bound caps exercised at 1k/5k with a registered `#[perf]` 1M probe. Remaining: multi-repo, broken-build, STM32/ESP-IDF/Zephyr corpora. |
| HAR-15 | External services | same contract test against fake and live | DONE. `scripts/harness/fake-live-equality.mjs`: one contract definition, four turn shapes (plain chat, named-tool-call turn, SSE stream with a deliberate mid-codepoint UTF-8 split, error shape — fake 429+Retry-After vs genuine live 4xx), documented normalization N1–N7 (bytes concatenated before parse, event-shape equality, schema-satisfying tool args, statusClass/messageClass errors, finish-reason classes). Fake leg runs offline in dogfood §7c; live leg is env-gated, cost-bounded (≤10 requests, `max_tokens ≤128`), SKIP-with-note without keys; `equality: equal` demonstrated against deepseek-chat. Selftest: 26 checks incl. torn-frame controls, schema-violation refusals, per-rule comparator probes. |
| HAR-16 | IDE drivers | JetBrains real-IDE UI tests on two newest IDE versions under real LAF | DONE. `faktor.ideaVersion` property (`apps/jetbrains/gradle.properties` pin `2024.1.7`, single source of truth) + `two-ide-matrix.sh`: the PIN leg runs the full visual certification (HostMatrix 1267/1267, canonical artifact byte-compatible); the ALTERNATE leg (`2024.1`, honest `sinceBuild=241` lower bound) runs the full structural matrix (1235/1235 — exactly the 32 edition5 visual-digest checks excluded) with `visualCertMode=structural` and FAIL-CLOSED refusals against re-pinning under a structural leg; artifacts suffixed `-<release>` (sanitized id, traversal refused) never clobber; dependency verification preserved (verification-metadata refreshed for the alternate download, restored trap-guarded; lockfiles never rewritten; missing refresh fails closed). Nightly `ui-smoke-jetbrains` gained a strictly additive second leg (env-driven `FAKTOR_JETBRAINS_ALT_IDEA_VERSION`; a failed leg exits before the lane marker). `check-gradle-integrity.sh` PASS; both legs ran green locally. Residual: alternate-release visual CERTIFICATION (a reviewed per-release baseline) is its own future row. |
| HAR-17 | Agent eval harness | 200-task corpus with hidden tests on real models | PARTIAL → HARNESS + REAL-MODEL LANE LANDED. `scripts/run-evals.mjs`: drives a throwaway daemon (temp data-dir; never the live data dir) through a recording proxy (hard `--max-requests` cap, default 20) over the conversation-quality suite (greeting, conceptual, code explanation + attachment, pronoun follow-up, instruction reversal, repo lookup, compare, return-to-topic, escalation, task follow-up). Gates are evidence-based: no-engineering-boilerplate in ask turns, cross-turn-reference (≥2 prior-reply tokens), engineering-state-only-at-escalation, escalation-established, request-material-recorded, cost-cap; review items (pronoun correctness, reversal compliance, permission auto-allows) NEVER fail the run. Real deepseek run recorded: 43 gates pass / 0 fail / 1 review, 10 turns, 19/20 requests; report template committed at `tests/eval/fixtures/eval-report.template.json`; gate selftest (6 planted violations) gated in dogfood §7c. Remaining: the full 200-task hidden-test corpus + nightly 30-task smoke (DOM-95). |
| HAR-18 | Hardware simulation | Renode/QEMU boards + HIL rig | DONE (emulated; physical rig half stays gated). `tests/hil/`: in-repo-built thumbv7m firmware (`#![no_std]`, vector table + LM3S6965 UART line protocol, pinned `sha256 6b448f49…`, detached workspace, clippy/fmt clean) runs under `qemu-system-arm -M lm3s6965evb`; `emulated-hil.mjs` drives the adapter path with typed failures (request-timeout, adapter-disconnect, protocol-violation, oracle-mismatch, image-mismatch, fault-not-typed) and per-scenario invariants (zero orphan qemu processes, wall-time bounds). Scenarios all PASS: happy, disconnect-mid-request, corrupt-image (integrity gate refuses by digest BEFORE spawn + direct boot fails typed), emulator SIGKILL → deterministic reboot (identical identity, seq=1), two-boot byte-identical determinism vs independent LCG oracle. Optional `.woodpecker/trusted/hil.yaml` manual lane (typed `emulator-unavailable` refusal on images without qemu — visible red, never fake green). The physical rig remains behind `scripts/programme/hil.mjs` (`FAKTOR_HIL=1`) by design. |

---

## 4. Detailed-table findings (pp.18–40)

The tables below enumerate every row the report says needs new proof. The
"Current" column is the repository state, not a restatement of the report.
Rows that are already fixed but still lack the report's proof are marked
PARTIAL; the full required test is in `programme.md` §4.

### 4.1 Core runtime suites (pp.18–22)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-01 | Session journal: random command sequences vs reference model; kill at every failpoint, replay, compare digests (p.18) | PARTIAL → reference model DONE (`session_modelcheck`: independent model, seeded sequences, journal-replay compare, crash-at-boundary OLD-or-NEW reconstruction, dispatch-marker refund semantics pinned; long campaign registered). Remaining: the every-durable-write kill matrix is bounded to the store seam points enumerated in HAR-04/06 (full SQLite-commit failpoint enumeration in flight with HAR-05). | S1 |
| DOM-02 | Typed ledger: corrupt a row inside the verified prefix; scheduled full sweep fails loudly; 1,000,000 entries open bounded (pp.19,4) | PARTIAL: incremental verify; no full sweep, no 1M test | S1 |
| DOM-03 | Admission: same submission from two IDEs; crash between receipt and spawn; replay after restart (p.19) | PARTIAL: `INV-TASK-START-IDEMPOTENT`, `INV-PROMPT-START-IDEMPOTENT`; no two-IDE/crash-between test | S1 |
| DOM-04 | SQLite: every version migrates; restore each backup and compare; busy/disk-full/corrupt-page; writer-queue concurrency (p.19) | PARTIAL: 313 store tests, plan tests, per-version migration matrix; no full schema compare, no store-fault injection | S1 |
| DOM-05 | Memory facts: plan test on every fact query; 1,000,000 churn rows keep activation/control/memory pages unchanged in result and time (p.19) | PARTIAL: the 1M test landed (PERF-09: control/identity/activation pages EXPLAIN-asserted SEARCH and p95-flat at 1,000,000 rows, 100k CI tiers) and the memory pagers are range-predicate + EXPLAIN-asserted at 200k churn; remaining: a plan test on every still-unaudited fact-query statement and 1M memory-page timing. Severity moved S0->S1. | S1 |
| DOM-06 | Checkpoints: generated trees with symlinks, permissions, binaries, empty folders, 1 GB files; one flipped byte refused; cleanup during concurrent snapshots (p.20) | PARTIAL: CAS fault campaign, `INV-CAS-COPY-TO-VERIFIED-STREAM`; no generated-tree/1GB/concurrent-cleanup suite | S2 |
| DOM-07 | Transactional editing: random concurrent writer always triggers stale-hash refusal; parse-before-accept per language; crash mid-write leaves old or new (p.20) | PARTIAL: edit fault campaign, 54 edit tests; no random concurrent writer/crash-at-write suite | S1 |
| DOM-08 | Git worktrees: two daemons on one repo; stale lock after crash; detached HEAD, submodules, sparse checkout, LFS pointers (p.20) | PARTIAL: 66 git tests, `INV-GIT-LEASE-UNREADABLE-NOT-STOLEN`; no two-daemon/stale-lock corpus | S2 |
| DOM-09 | Workspace files/watchers: symlink and junction escapes per OS; 1,000 shadowed turns with real watchers; in-use durable root never evicted (p.20) | DONE for the shadowed-turn claim: `real_watcher_shadow_churn_keeps_registry_and_eviction_bounded` (128-cycle fast tier) and `fault_probe_1000_shadow_turns_with_real_watchers_census_os_resources` (1,000 cycles on the REAL watcher backend: OS thread+fd census returns exactly to baseline 5/4 after settle vs >500 threads historically, singleton spawn reuse, eviction bound 2/insert, durable root survives and stays writable; deterministic counters, no wall-clock deadlines). Symlink/junction escape corpora remain per-OS residual work. | S1 |
| DOM-10 | Bounded context: 10,000-turn soak with bounded growth; compaction never drops immutable instructions or task state; sizes 0/1/2 refused typed (p.21) | DONE. 1k and 10k turn soaks force 50/500 real compactions with the configured minimum reduction held every time (min 0.250/0.252), retained <=3500 tokens/227 rows, goal/criteria/decision markers on every compacted wire, and task/memory/learning/pinned state byte-identical; 0/2/3/127/128 token size matrix refuses typed below the floor with zero compaction-model calls (1-token is wire-unconstructible and pinned at the durable layer) and journals CompactRejected at the floor; non-ignored 10k tier runs 8.6s. | S1 |
| DOM-11 | Retrieval/index: labelled queries per fixture repo with recall@10 floors; renaming a symbol moves results; deterministic eviction under caps (p.21) | PARTIAL: production-wiring retrieval tests, tree-sitter for Rust/Python/TS/JS; no labelled recall corpus | S2 |
| DOM-12 | Scheduler: random DAGs; concurrency check of budget accounting; breaker state-machine model (p.21) | PARTIAL: DAG campaign, 71 tests; no random DAG/concurrency/model check | S2 |
| DOM-13 | Process supervision: kill daemon mid-build, scan process table per OS; double-fork and fork-bomb children contained (p.21) | PARTIAL: 173 terminal, 107 PTY, 8 Job Object tests; no kill-mid-build process-table scan | S1 |
| DOM-14 | Shutdown: 1,000 start/stop cycles with random work in flight; thread/handle/child counts return to baseline (p.21) | PARTIAL: `INV-SHUTDOWN-ZERO-LIVE-AUTHORITIES`; no 1,000-cycle resource census | S2 |
| DOM-15 | Memory/instructions/learning: unverified failure never becomes a learning; unused skills cost zero tokens; workspace memory never leaks (p.22) | PARTIAL: `INV-INSTRUCTION-ROOT-AUTHORITY`, 50 instruction + 43 learning tests; no negative-learning/skill-token/workspace-leak test | S1 |

### 4.2 Providers, tools and safety (pp.22–27)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-16 | Model transports: one scripted conversation per family normalizes to equal events; chaos proxy; weekly live contract (p.22) | PARTIAL: recorded fixtures per family; no cross-family differential or chaos proxy | S1 |
| DOM-17 | Model catalog/probe: endpoint lies, times out, returns 10,000 models, renames a model mid-session; Ollama `/api/tags` empty/huge (p.23) | PARTIAL: catalog/probe route exists, `foundation_rev8_contracts_tests.rs` covers absent connection; no hostile catalog corpus | S1 |
| DOM-18 | Model parameters: every parameter × family, unset omitted, zero sent, unsupported refused before sending; compiled request byte-checked (p.23) | PARTIAL: `model-parameters/compile` route, A03/A04; no parameter × family matrix | S1 |
| DOM-19 | Locality: 127.0.0.1, ::1, 10/8, 192.168/16, public IP, DNS rebinding; private-LAN policy (p.23) | PARTIAL: A07/A08, control P-09 audited missing; no endpoint corpus/rebinding test | S1 |
| DOM-20 | Routing: replay real outcome logs; never pick a model another beats on both cost and quality; tail vs full history same decision (pp.23–24,4) | PARTIAL: router property crate, `INV-MAX-QUALITY-ONE-AUTHORITY`, 64-call tail; no decision-equivalence test | S1 |
| DOM-21 | Vision/attachments: malformed image headers, oversize, per-family media encoding, attachment deleted between admission and send (p.24) | PARTIAL: production-wiring vision test, A14; no hostile media corpus | S1 |
| DOM-22 | Embeddings: slow, wrong dimension, NaN, model change mid-index (p.24) | PARTIAL: `no_embedder_degrades_honestly...`; no hostile embedder suite | S2 |
| DOM-23 | MCP: misbehaving-server corpus (garbage frames, hangs, crash loops, oversize), OAuth expiry/revoke, tool list changes mid-turn (p.25) | PARTIAL (crash + stderr classes closed): MCP children now ALWAYS drain stderr into a bounded ring (a verbose server can no longer deadlock on a full pipe — proven by the flood test answering under continuous stderr pressure), a server exiting at the handshake is the same transient class as LSP (retryable, one bounded restart, stderr evidence, EPIPE sub-class included), and server-answered RPC errors stay permanent (`initialize_child_exit_...`, `initialize_crash_once_recovers_after_one_restart`, `initialize_server_rpc_error_stays_permanent`, `verbose_stderr_is_drained_and_never_blocks_the_server` + the existing garbage/hang corpora). OAuth expiry/revoke and mid-turn tool-list changes remain. | S1 |
| DOM-24 | LSP/ACP: server crash during request; 10 MB diagnostics; duplicate frame headers (p.25) | PARTIAL (crash class closed): a language server that EXITS mid-request is now an exact transient class — EOF marker → retryable error carrying the drained stderr tail, one bounded restart per start (both exit-before-answer and EPIPE-before-read), server-answered RPC errors stay permanent, and the wire's 409 mirrors `retryable`. Pinned by `initialize_child_exit_is_transient_retryable_with_stderr_and_one_restart`, `initialize_early_exit_before_read_is_transient_retryable`, `initialize_crash_once_recovers_after_one_restart`, `initialize_server_rpc_error_is_permanent_and_not_retried` and the route's retryable mapping test. 10 MB diagnostics / duplicate-frame corpora remain. | S1 |
| DOM-25 | Terminal/shell: commands that write outside workspace, start background daemons, change directory, print 1 GB (p.25) | PARTIAL: `INV-SHELL-*`, 173 terminal tests; no hostile command corpus | S1 |
| DOM-26 | Browser authority: hostile pages (redirect chains, third-party iframes, service workers, WebRTC); IDN/IP literal/userinfo URL tricks (p.26) | PARTIAL: 8 `INV-BROWSER-*`; X-06/X-08/X-09 audited missing | S1 |
| DOM-27 | Sandbox/permissions: escape corpus per OS (symlink races, hard links, traversal, `/proc`, env injection); OS denial asserted per platform (p.26) | PARTIAL: sandbox/adversarial tests; no per-OS escape corpus | S1 |
| DOM-28 | Network egress: 50+ planted key formats in files/env/tool output blocked; DNS rebinding; redirects to private addresses (p.26) | PARTIAL: `INV-NETWORK-AUTHORITY-SINGULAR`, destination-policy fuzzing; no planted-secret corpus/rebinding | S1 |
| DOM-29 | Prompt injection: corpus across every input channel; no permission/instruction/tool change; rerun weekly with real models (p.27) | PARTIAL: security suite/sourcing certification; no channel corpus/real-model rerun | S1 |
| DOM-30 | Hooks: timeouts, crashes, malicious output, a hook that edits files; hook test/delete routes (p.27) | DONE for the route half. `hook_test_and_delete_routes_run_the_real_registry` drives the real registry through HTTP: untrusted save/test → 409 hook_untrusted; trusted test runs the real hook; >100-byte output truncated with visible ellipsis; a `sleep 5` hook under a 100 ms deadline returns warn in <3 s (timeout class); unknown event/field 400; unknown id 404; delete unregisters (replay 404); audit 200. Crash-class hook campaigns remain unit coverage. | S1 |

### 4.3 Engineering pipeline and proof (pp.27–30)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-31 | Task contract: golden contracts for 30 fixture goals; rewording must not drop a constraint (p.27) | PARTIAL: `INV-CONTRACT-*`; no golden corpus/rewording eval | S2 |
| DOM-32 | Settlement identity: interleaving at every await point; queued/recovery/watcher paths; static scan for lazy task-id reads (pp.27–28) | PARTIAL: see AG-01 | S0 |
| DOM-33 | Task/run state: exhaustive pair table served by daemon and rendered identically in both IDEs (p.28) | PARTIAL: see AG-13 | S1 |
| DOM-34 | System-intelligence registry: byte-identical contract with/without provider; hostile findings (wrong snapshot, oversize, invalid confidence) (p.28) | PARTIAL: `INV-INTELLIGENCE-TIER-BOUNDS`, 102 semantic tests; no absent-provider byte contract/hostile findings | S1 |
| DOM-35 | Verification registry/Kiwi: forged verdicts (wrong identity, stale snapshot, oversize evidence) refused; Kiwi down mid-verification (p.28) | PARTIAL: `INV-VERIFICATION-REQUIRE-PROVIDER`, `INV-KIWI-OPTIONAL-ADAPTER`; no forged-evidence suite | S1 |
| DOM-36 | Proof surfaces: totals at 0/1/499/500/501/10,000; broken chain never renders verified (p.28) | PARTIAL: see AG-10 | S2 |
| DOM-37 | Completion rules: traps — tests pass but criterion unproven; proof on stale tree; reviewer approves everything — each blocks (p.29) | PARTIAL: completion-review blocking, typed criterion proofs, A35; no trap corpus | S1 |
| DOM-38 | Completion steps: crash between push and PR; rerun exactly one PR; branch-protection refusal; token expiry mid-step (p.29) | PARTIAL: completion contract tests, fake GitHub; no crash/rerun/refusal suite | S1 |
| DOM-39 | Changes/review/restore: preview then restore exactly the previewed tree; manual edit between preview/restore refused; comments survive restore/fork (p.29) | DONE for preview/restore/accept/review comments; FORK residual. `restore_preview_restore_and_review_comments_are_scoped` (preview writes nothing; write action; manual edit between preview/restore → 409 checkpoint_conflict with stale hash; malformed hash 400; restore lands the exact recorded tree; comments survive restore; foreign session byte-identical 404; comment revision fences) and `plain_change_accept_is_idempotent_and_rejects_foreign_paths` (identical replay returns the stored receipt — PRODUCTION FIX: the route now consults the durable receipt before the shadow-presentation refusal; different selection 409; `../escape.txt` refused). Residual: fork does not copy review comments (session-keyed store; needs foundation/review + fork wiring). | S1 |
| DOM-40 | Child agents: random delegation trees never exceed root budget; steer/pause/resume/retry/stage/merge concurrent; parent crash leaves no running children (p.29) | PARTIAL: `INV-CHILD-*`, kind-filtered control reads, A25/A26; no random-tree/concurrency/parent-crash suite | S1 |
| DOM-41 | Tournament/board/integration: owner edit during landing; disagreeing candidates; verification on merged tree (p.30) | PARTIAL: tournament/integration-record tests; no owner-edit/disagreement suite | S1 |
| DOM-42 | Budgets/settlement: cap of "1" on every entry path; unknown pricing never zero; crash between reservation/settlement; micro-unit rounding (p.30) | DONE. Every cap entry point validated: task start (single + plain prompt), tournament, agent budget, session budget PUT and model comparison. The two ledger entry points now refuse `Some(0)` typed (zero reads as unlimited at the store; `None`/clear is the only removal) and refuse caps above `i64::MAX` instead of silently clamping; the comparison route gains the shared money decoder and refuses a positive shared cap smaller than the candidate count (1 micro over 2 candidates previously split to 0 = unlimited). Tests: `every_cost_cap_entry_refuses_zero_and_over_domain_and_enforces_one_micro`, `child_cost_cap_entry_validates_the_domain_before_any_child_effect`, `session_budget_entry_refuses_ambiguous_zero_negative_fractional_and_over_max`, `model_comparison_refuses_a_budget_too_small_to_fund_each_candidate` (all boundary 0/negative/fractional/max), `settlement_rounding_loop_never_truncates_to_zero_or_understates` (1 ppm price, remainders, repeated halving, i64::MAX), `crash_recovery_loop_never_double_charges_and_never_loses_a_reservation` (12 crash/reopen rounds: reservation survives, pre-dispatch refund, post-dispatch UNCERTAIN, finalize once), plus the existing cap-1 real-drive E2E and unknown-price typed settlement tests. 36 budget tests green. | S0/S1 |
| DOM-43 | Engineering outcomes: concurrent settlement/executor write same record; VCSC denominator over failed/unmeasured cohorts (p.30) | PARTIAL: outcome ledger tests, A36; no concurrent writer/VCSC cohort test | S1 |
| DOM-44 | Worker plane: network partition; lease-expiry races; malicious worker forged results (p.30) | PARTIAL: 9 `INV-WORKER-*`; no partition/race/forged-result suite | S1 |

### 4.4 Sessions, history, transcript and search (pp.30–31)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-45 | Create/delete/export/fork/metadata: delete during running turn; export/import round-trip; every export scanned for planted secrets; fork then edit both sides (pp.30–31) | DONE for delete/export/metadata; IMPORT residual. `session_delete_export_and_metadata_routes_are_typed`: meta defaults/pin/archive/rename; blank title 400; wrong type 400; missing 404; delete during a live turn without abort → 409 session_active; NUL reason 400; abort delete → 200 with purge counts (PRODUCTION FIX: abort lands ReadyForNextTurn which the deleter counted active — the route now moves the session to a non-active lifecycle before deleting); replay idempotent; export schema; export after delete 409 session_deleted; fork smoke. Residual: no session-import route exists, so an export/import round-trip cannot be exercised. | S1 |
| DOM-46 | History listing: update during walk; 2,000 and 20,000 conversations; plan test global/per-workspace (p.31) | DONE. `twenty_thousand_conversation_history_walk_is_ordered_once_and_page_bounded` (20,000 global + 2,000 workspace rows, page-bounded, exactly once, snapshot bound, post-snapshot insert excluded) and `history_listing_statements_are_index_backed_without_temp_sorts_or_scans` (EXPLAIN: SEARCH on idx_session_listing_revision_id / idx_session_workspace_listing_revision_id / COVERING for the snapshot; no SCAN, no TEMP B-TREE); concurrent-update walk already pinned by `same_millisecond_update_during_a_walk_cannot_skip_or_duplicate` + `concurrent_title_updates_allocate_unique_monotonic_revisions` + `keyset_walk_is_stable_under_inserts_and_hard_deletes`. | S1 |
| DOM-47 | Message pages: random appends during walk; `before` at 0, 1, newest seq, past it (p.31) | DONE. `message_before_walk_survives_concurrent_appends_and_middle_deletes`: deterministic interleaved appends during the walk, middle-range and already-served deletes, and boundary probes at before=0/1/2, newest, newest+1, far past tail, limit=0 — no skip/duplicate of surviving rows, no resurrection, bounded pages, termination. (The 100k claim is the scale-probe transcript workload; no 100k page walk exists at this HEAD.) | S2 |
| DOM-48 | Transcript search: reference oracle over visible decoded text; JSON-key/quote queries; Unicode folding; 100,000 rows within page bound (p.31) | DONE. `transcript_search_agrees_with_an_independent_visible_text_oracle` compares production search against an independently implemented visible-text oracle across escapes (\u00e9/\u0061/\n/\/), Kelvin/U+0130 folding, emoji/CJK, raw JSON keys and hidden fields, tool-result excerpts, boundary queries and page limits — and it FOUND and fixed a real prefilter defect: SQL candidates now admit payloads containing backslashes or non-ASCII text so escape-only and non-ASCII-folding matches are never hidden (50k streaming probe stays green). | S1 |
| DOM-49 | Transcript rendering: window-edge orphan; every outcome × exit-code; 16 sessions streaming with no cross-session text (p.31) | PARTIAL: denied-call merge; no window-edge/outcome matrix/16-stream test | S1 |
| DOM-50 | Switching conversations: switch during streaming, approval prompt and compaction (p.31) | DONE. Webview selftest drives a switch during streaming, an approval prompt and an active compaction: old run/pending-approval/session panels drop on id change, org/daemon-scoped panels survive, the old approval card goes un-actionable, and host fences (bumpLifecycle, streamRefresh.reset, activeRunId=null, taskStartGate.reset, stream stop, sessionId/generation fences) are pinned; selftest 328/0. | S1 |
| DOM-51 | Turns/usage/efficiency: per-session totals equal provider calls for 1,000 turns; mixed known/unknown pricing (p.31) | DONE for the route half. `usage_totals_match_the_provider_ledger_and_efficiency_stays_honest`: 1,000 durable provider-call rows — session usage totals equal the ledger sum exactly and the global totals agree; efficiency reports tokens/cache/localOperatingCost/routing outcomes as `unavailable` (never a fabricated zero); a cap-less task reports maxCostMicro null; hostile ids/query typed. Unknown-price settlement visibility is now covered by the unpriced-call usage unit (DOM-42 wave). | S2 |

### 4.5 Configuration, credentials and settings (pp.32–34)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-52 | Config descriptors/search/provenance/snapshot: every key × machine/user/workspace/session; winner + provenance asserted; search finds every key (p.32) | PARTIAL: config routes, 7 foundation config tests; no layer matrix | S1 |
| DOM-53 | Preview/commit/reset/rollback/import/export/runtime ack: stale revision conflict; crash mid-commit; hostile import; rollback each revision; ack only after restart-required keys apply (p.32) | DONE for the route half (ack-gating residual). Wrong-schema import → 400 with revision/values byte-identical; valid import atomic; rollback replays exact values as a new revision; stale rollback 409; roll-forward 400; stale ack 409; current ack echoes runtimeRevision. Residual: the ack route acks any current revision (it does not itself gate on restart-required keys); crash-mid-commit stays store crash-arm coverage. | S1 |
| DOM-54 | Session settings: per-session override applies next turn only; cleared on reset (p.33) | DONE. Route boundaries prove the advertised contract: contextWindow 255 and 10,000,001 → 400 while 256 → 200; maxTurns 0 → 400; unknown key/unset → 400; applyTiming advertises contextWindow=next_turn / maxTurns=next_task; unset clears the override. The next-turn consumption itself is pinned by the agent-side apply tests. | S1 |
| DOM-55 | Credential vault: planted-secret scan of every output channel; rotate during in-flight call; delete then use refused typed; OS keychain unavailable (p.33) | DONE. Route suite covers store/rotate/list/delete with opaque-reference-only responses; malformed/empty/oversized/secret-in-label bodies are typed 400; a foreign handle is byte-identical to a missing handle on delete and rotate (vault 404 text no longer echoes the handle); delete-then-use is typed 409 (probe), binding a deleted handle is a typed 400 at save time, and rotate-after-delete refuses, never resurrects. Rotation during an in-flight probe is generation-fenced: the probe resolves generation + bytes once, and `mark_verified_generation` refuses to verify a generation that rotated mid-call (late old-generation success leaves the new material unverified). A planted secret is scanned across credentials list, provider connections, catalog probe, config snapshot/export, diagnostics preview/export and session export. An unavailable machine vault (file occupying the vault root; no OS-keychain dependency) is a typed 500 `store` with no reference row. Tests: `credential_routes_store_rotate_list_delete_and_never_echo_the_secret`, `malformed_bodies_are_typed_400_and_store_nothing`, `foreign_and_missing_handles_are_indistinguishable_at_the_wire`, `unavailable_machine_secret_store_is_a_typed_500_never_a_fake_handle`, plus vault unit `foreign_and_missing_handles_are_indistinguishable_byte_for_byte`, `delete_then_use_and_rotate_are_typed_and_never_resurrect`, `unavailable_secret_store_degrades_typed_and_writes_no_row` and `rotate_during_an_in_flight_probe_is_generation_fenced`. | S0 |
| DOM-56 | Provider connections: endpoint down, invalid TLS, wrong key, slow responses; delete a connection a session is using (p.33) | DONE for the route half (bind residual). `provider_connection_test_health_and_delete_are_bounded_and_typed`: closed loopback → observed failed; health before probe 404 no_observation, after 200 failed; plaintext behind https → failed (invalid TLS, no panic); 401 with a stored wrong key → authentication_required with the failed stage; hostile tls_mode refused; delete 200 (remoteRevoked:false), replay/test/health 404, sessions unaffected. Residual: no route binds a session to a connection, so deleting an in-use connection has no representable state. | S1 |
| DOM-57 | Service executable trust: binary swapped between check/start; symlinked path; updater replaces binary (p.33) | PARTIAL: O-04/O-05; no swap/symlink/updater race test | S1 |
| DOM-58 | Workspace binding: two windows on one folder; renamed folder; network drive; case-insensitive FS (p.33) | PARTIAL: S31; no multi-window/network/case test | S2 |
| DOM-59 | Diagnostics: planted secrets absent from bundle; size cap holds on 10 GB data folder (p.33) | PARTIAL: preview/export routes; no secret/size test | S1 |
| DOM-60 | Telemetry: zero network calls while off; preview equals what is sent (p.34) | MISSING: audited as missing | S2 |

### 4.6 Foundation services (pp.34–35)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-61 | Instructions: precedence matrix; conflicting rules; hostile/1 MB imports; preview equals sent request (p.34) | PARTIAL: I controls audited partial | S1 |
| DOM-62 | Memory: after delete absent from every store/index/export/model context; retention boundary; workspace scope never leaks (p.34) | DONE. `memory_delete_is_absent_from_every_view_and_restore_revives_it`: a workspace record visible to a sibling session; after delete it is absent from the records view, raw /native/memory, context export and the next-turn context preview; stale restore version 409, correct restore revives as version 3; retention -1/3651 days → 400 with a 0-delete dry run; legacy delete CAS/replay semantics. No production fix needed (the raw view already hides tombstones). | S1 |
| DOM-63 | Skills: hostile archives (traversal, symlinks, huge files); untrusted skill refused; trust revoked on file change; 600 skills reachable (p.34) | DONE for the on-disk tree half (archive residual). 600+ discover with a hostile tree (symlinked skill skipped); limit=10000 serves ≤500; 257-byte cursor 400; symlink/unknown install 404; a 1 MiB SKILL.md installs ≤64 KiB; untrusted invoke 409 skill_untrusted; trust then substitution; hostile variable names, 4,097-byte values and 33 variables 400; invocation policy accepted/rejected; get; delete then 404. Residual: no route accepts tar/zip input; hostile archive shapes are exercised as on-disk trees. | S1 |
| DOM-64 | MCP servers: start twice; stop during call; connect with expired token; resources/prompts (p.35) | DONE for the route half. Real supervised stdio mock through HTTP: start twice returns the identical catalog; tools/resources 200; stop racing a catalog read is bounded and both then 409 mcp_not_ready; double stop reports stopped:false; disabled/untrusted 409; unknown 404; an expired OAuth bundle on a saved remote record → 409 oauth_expired BEFORE any network; delete then start 404. Residual: no MCP tool-call route exists, so stop-during-call is exercised as stop-during-catalog-read. | S1 |
| DOM-65 | Presets/agents: fallback cycles; deleted model in chain; envelope caps on children (p.35) | DONE for the route half. Unknown-provider role → 409 unknown_provider; valid role served with applyTiming; duplicate candidate chain 400; unknown-provider chain 409; order preserved; a durably seeded chain naming a vanished provider reads served:false and cannot be re-saved; empty chain clears; unknown preset 404 on GET/POST. Residual: environment overrides live in CLI provider-env handling and have no route surface. | S1 |
| DOM-66 | Permissions/approvals: rule-matching corpus (quoting, env vars, `&&`, subshells, Unicode look-alikes); preview equals decision; double reply applies once; expired approvals (p.35) | DONE for delete + matching corpus. `permission_rule_delete_and_matching_corpus_refuse_compound_targets`: an exact rule allows only the exact canonical target; appended &&, ;, alternate quoting, $(...), env var, trailing space, Unicode quote, NBSP and embedded newline all stay ask/unmatched; path globs are whole-string; open-ended allow glob 400; expired rule 400; audit records source=preview; delete missing revision 400, stale 409, correct 200, replay 404 and preview returns to ask. Double-reply/expired-approval behavior stays permission-route/unit coverage (not duplicated). | S1 |
| DOM-67 | Context: pins survive compaction; manifest totals equal parts; cancel mid-compaction leaves original (p.35) | DONE. Pinned decision/learning/task rows byte-identical across real compactions and carried in report.pinned; manifest hardening in production (each part hash-verified against its declared size via a bounded read, total_bytes required and equal to the sum) with 6 forgeries refused (lying total/size, missing total, truncated, wrong version, empty) and refs replaced by archive-evicted markers; deterministic mid-compaction cancel leaves the page byte-identical with checkpoint None and zero journal events; truncated checkpoint is typed Malformed, never silent loss. | S1 |

### 4.7 Enterprise, cloud, billing, workers, updater (p.36)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-68 | Organizations/members/invitations/SSO: role × route matrix; invitation reuse/expiry; OIDC state/nonce replay; clock skew; IdP down (p.36) | PARTIAL: 219 cloud tests, SSO tests; no matrix/replay | S1 |
| DOM-69 | Enterprise settings/effective config/audit: override attempts at every layer; append-only audit chain tamper check (p.36) | PARTIAL: enterprise routes; no override matrix/chain tamper | S1 |
| DOM-70 | Retention/deletion/tombstones: crash mid-advance; GC racing a new retention hold; tombstone lists exactly what was purged (p.36) | PARTIAL: deletion/GC routes; no crash/race/tombstone test | S1 |
| DOM-71 | Billing/credits/entitlements/usage/outcomes: concurrent credit grants; `i64::MAX`; usage fold equals settlement ledger (p.36) | DONE. Concurrent grants are exact and race-safe: 8 writers x 40 grants plus an immediate retry of every idempotency key leave the balance equal to the distinct sum (no lost update, no double debit) and survive reopen; `i64::MAX` is accepted twice exactly, a third grant is a typed `granted_micro` overflow and `i64::MAX + 1` is typed Malformed, with the balance unchanged. The durable usage fold is byte-identical (serialized) to the independent pure fold over 6 concurrent writers appending base and correction rows while folds run, and the same bytes survive a crash/reopen. Tests: `concurrent_credit_grants_are_exact_and_idempotency_is_race_safe`, `i64_max_credit_grants_overflow_typed_and_never_clamp`, `usage_fold_matches_the_ledger_byte_for_byte_with_concurrent_writers_crash_and_corrections`; existing correction-chain/rebuild/restart coverage stays green. | S0 |
| DOM-72 | Workers/jobs: token replay; heartbeat after revoke; result for stale generation (p.36) | PARTIAL: 9 `INV-WORKER-*`; no replay/revoke/stale test | S1 |
| DOM-73 | Updater: tampered package; power loss at each apply step; downgrade refused unless allowed; update while a turn runs (p.36) | PARTIAL: 165 updater tests; 8 CLI subcommands run by no test | S1 |
| DOM-74 | SCM webhook/GitHub App: forged signature; replayed delivery; installation id mismatch (p.36) | PARTIAL: SCM invariants; no forged/replay/mismatch test | S1 |
| DOM-75 | Control-plane sign-in: sign out while offline; token expiry mid-call (p.36) | PARTIAL: VS Code sign-in commands; no offline/expiry test | S2 |

### 4.8 Sourcing engine (pp.37–38)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-76 | Quote at quantity: brute-force reference over generated offers; doubling quantity never raises unit price within one packaging when breaks are monotone (p.37) | PARTIAL: 225 commerce tests incl. plausible-wrong-price; no brute-force/generated property | S1 |
| DOM-77 | Part matching: suffix corpus from real catalogues; normalizing twice equals once (p.37) | PARTIAL: `INV-COMMERCE-MPN-MATCH-EXACT`, `INV-COMMERCE-PART-IDENTITY`; no corpus/idempotence test | S1 |
| DOM-78 | Money: format→parse round-trips for every currency/scale (p.37) | PARTIAL: float scan, money tests; no currency/scale round-trip matrix | S1 |
| DOM-79 | Lazy exposure: near-miss phrases (a Rust supplier trait) and real chat logs added to trigger corpus (p.37) | PARTIAL: release-blocking 10,000-turn token test; no near-miss/log corpus | S1 |
| DOM-80 | Browser authority: hostile pages (redirect chains, third-party iframes, service workers, WebRTC) (p.37) | PARTIAL: 8 `INV-BROWSER-*`; no hostile-page corpus | S1 |
| DOM-81 | Vendor changes: mutate every recorded fixture (renamed keys, moved fields, swapped types); drift or correct parse (p.38) | PARTIAL: offline fixture replay; no mutation campaign | S1 |
| DOM-82 | Live accuracy: weekly canaries; recorded real basket totals (p.38) | MISSING: opt-in manual canaries only | S1 |

### 4.9 Protocol, CLI and IDE surfaces (pp.39–40)

| ID | Feature / required proof | Current | Severity |
|---|---|---|---|
| DOM-83 | Native protocol: route template for all 246 routes; plugin versions N-1/N-2 against current daemon (p.39) | PARTIAL: generated DTO parity, frozen contracts; no per-route template | S1 |
| DOM-84 | Health/readiness: ready stays 503 through slow recovery; health never flaps (p.39) | PARTIAL: health/ready routes; no slow-recovery test | S1 |
| DOM-85 | Event streams: reconnect at every event id; frames larger than bound; consumer that never reads (p.39) | PARTIAL: stream probes, 16-stream perf workload; no reconnect-at-every-id/never-read test | S1 |
| DOM-86 | Legacy catalog routes: any other non-native path from the client refused typed (p.39) | PARTIAL: narrow client allowlist; no negative-path test | S2 |
| DOM-87 | CLI: template for all 43 commands (p.39) | PARTIAL: 398 CLI tests, doctor exit codes; no per-command template | S1 |
| DOM-88 | VS Code webview lifecycle: message before listener, hidden reveal, reload, rapid reveals; unknown patch type counted, never dropped (p.39) | PARTIAL: handshake/backpressure fixes; no adversarial lifecycle test | S1 |
| DOM-89 | Controls/screens: per-control contract tests derived from each record; real IDEs, not only webviews/offscreen Swing (p.40) | PARTIAL: 200/200 exercise, 72/72 screens, visual contracts; no per-control contract tests in real IDEs | S1 |
| DOM-90 | Workflows/acceptance: daemon crash and network loss mid-flow; one adversarial variant per scenario (p.40) | PARTIAL: W01–W16 and A01–A40 gates; no crash/network-loss/adversarial variant | S1 |
| DOM-91 | Visual baselines: missing reviewed baseline fails; baseline changes ship rendered differences + reviewer (p.40) | DONE for the write-policy half: `--bootstrap-baselines` is gone from the PR and trusted lanes (comparing runs never write a baseline; a missing reviewed baseline fails loudly), the scale baseline is created only by the explicit `--write-baseline` flag with missing/corrupt failure paths proven, and both per-platform baselines are committed (linux measured on the CI execution class). Reviewer-signoff process for baseline diffs remains a process residual. | S1 |
| DOM-92 | Screen readers: scripted NVDA, VoiceOver, Orca spoken output per workflow (p.40) | MISSING: automated name/role only | S2 |
| DOM-93 | Install/update: update while a run is active; rollback after failed update; downgrade refused (p.40) | PARTIAL: install matrix, tamper test, 165 updater tests; no active-run/rollback/downgrade lane | S1 |
| DOM-94 | Platforms: Windows JetBrains visual records; real-IDE lanes on macOS and Windows (p.40) | PARTIAL: Edition 8 Windows records exist; primary `visual-baselines.json` lacks Windows; real-IDE lanes missing | S2 |

### 4.10 Six-layer roll-up (p.2 diagram; pp.10–17)

The report's six layers are Unit and property, State model and crash,
Contracts, IDE and screen readers, Agent evals, and Hardware. The findings
above roll up as:

- **L1 Unit and property — PARTIAL (narrowed).** 6,880 Rust tests and 158
  invariant mutations are real; proptest with demonstrated shrinking and
  persisted-seed replay landed in the property crates (HAR-07). The 80%
  mutation floor remains missing (LANE-05).
- **L2 State model and crash — PARTIAL (much narrowed).** Fault campaigns
  and the journal/ledger/CAS invariants exist; fs durability boundaries are
  now failpoint-complete with build-failing drift scans (HAR-04 fs half);
  exhaustive interleaving + crash-seam enumeration landed (HAR-06/08); the
  reference session state machine landed with a non-vacuous long campaign
  (HAR-09: 73 seam hits/run, model-corrected dispatch-marker refund
  semantics); store-fault injection remains in flight (HAR-05).
  Kill-at-every-SQLite-commit is the remaining delta of HAR-04.
- **L3 Contracts — PARTIAL (much narrowed).** Transport chaos (HAR-03),
  secret-scrubbing record/replay (HAR-02) and the fake-vs-live equality
  lane (HAR-15) all landed with selftests gated in dogfood §7c; weekly
  vendor/MCP/SCM canaries remain (DOM-16/23/78/82).
- **L4 IDE and screen readers — PARTIAL (narrowed).** Structural VS Code/
  JetBrains gates exist; the JetBrains two-IDE-version real-host matrix
  landed (HAR-16: pin full certification + alternate structural, additive
  nightly leg); real screen readers and Windows JetBrains visual records
  remain (AG-16, REG-07/08, VIS-02).
- **L5 Agent evals — PARTIAL.** The eval harness and real-model
  conversation-quality lane exist (`scripts/run-evals.mjs`, cost-capped,
  evidence-based gates, real deepseek run green: HAR-17); the 200-task
  hidden-test corpus and nightly 30-task smoke remain (DOM-95).
- **L6 Hardware — PARTIAL.** Emulated HIL landed (HAR-18: QEMU + in-repo
  pinned firmware, five fault scenarios, orphan/runtime invariants,
  optional manual CI lane); the physical rig stays gated behind
  `scripts/programme/hil.mjs` and part-change rows remain (DOM-96).

Failure of any layer blocks release at that layer's lane; a green lower
layer never excuses a missing upper layer (p.2: "every promise ships only
when its tests pass in every lane at one commit").

---

## 5. Performance, evals, revenue, lanes, dogfood, visual, hygiene

### 5.1 Performance (p.41)

| ID | Workload / gate | Current | Severity |
|---|---|---|---|
| PERF-01 | 1,000 prompt turns + forced compaction: turn curve ratio ≤20 overall, ≤2 steady, p95 ≤15 s | DONE. Script asserts exactly; runs in CI in the nightly `scale` lane (`--turns 1000`) plus the committed scale-baseline curve comparison. | S1 |
| PERF-02 | 100,000 messages: every seq once; p95 page ≤500 ms; page curve ratio ≤6; search ratio ≤25 | DONE. Script asserts; runs in CI in the nightly `scale` lane plus the baseline curve/latency comparison. | S1 |
| PERF-03 | History walk 2,000: every conversation once including concurrent updates | DONE. The 2,000-conversation walk is the scale-probe `history-2000` workload (nightly scale lane), and the listing half is proven at the store: revision-ordered snapshot-bounded keyset walk that cannot skip/duplicate under same-millisecond updates, concurrent title updates, inserts and hard deletes, plus 20,000- and 1,000,000-conversation walks (`session_listing_tests`). | S1 |
| PERF-04 | 5,000 memory facts with edits/deletes/retention: counts exact after each operation | DONE. Script asserts; runs in CI in the nightly `scale` lane. Retention verifies the durable end state (all tombstones removed, live set untouched) after a bounded typed-lock retry; transient SQLITE_BUSY/backpressure now surfaces retryable:true on the memory and foundation store routes via the store's transient-contention classifier (typed + message forms, adversarial mapper tests), while corrupt/malformed stay terminal. | S2 |
| PERF-05 | 500 child agents: one projection read, exact counts | Scale probe; runs in CI in the nightly `scale` lane. | S2 |
| PERF-06 | 10,000 claims: totals exact, page bounded | Scale probe; runs in CI in the nightly `scale` lane. | S2 |
| PERF-07 | 500 MCP servers, 500 skills, 500 hooks: every item reachable, listing bounded | Scale probe; runs in CI in the nightly `scale` lane (save latencies tracked as p50 with lock-shim time excluded). | S1 |
| PERF-08 | 16 concurrent live event streams: no cross-session frames, bounded queues | Scale probe; runs in CI in the nightly `scale` lane (stable submission id, bounded lock retry). | S1 |
| PERF-09 | 1,000,000 internal fact churn: point lookups and kind pages flat; plan tests green | DONE. Child control queue + identity and agent tool-activation lookups proven index-flat at 1,000,000 churn rows (100k CI tiers + 1M [fault] probes; EXPLAIN SEARCH on sqlite_autoindex_memory_fact_1 / idx_memory_fact_kind_updated, any SCAN memory_fact fails); p95 flat vs zero-churn baseline (median 10us->11us session, 2.4us->2.8us agent); hostile 5,001-row control space still refuses typed at scale and corrupt/missing activation rows stay typed ReadFailed/Absent; green in 2.3s/4.7s. | S0/S1 |
| PERF-10 | 1,000 shadowed turns per OS with real watchers: thread/handle counts flat | DONE. 128-cycle fast tier and a 1,000-cycle [fault] probe run on the REAL watcher backend (registration-deadline path, not the degraded preflight): macOS/Linux thread+fd census returns exactly to baseline (5/4) after settle versus >500 threads historically, singleton-worker spawn reuse, registry <= MAX_CACHED_ROOTS, eviction-candidate probe <=2/insert, durable root survives and stays writable; deterministic counters only, no wall-clock deadlines; 46s green. | S1 |
| PERF-11 | Every workload records RSS; growth beyond reviewed per-platform baseline fails | PARTIAL. Daemon RSS start/peak/end is sampled over the canonical run with absolute 512 MiB growth bounds and compared against the reviewed per-platform section of `scripts/edition8/scale-baseline.json` (regressions beyond per-metric multiplier+floor fail naming metric/baseline/current/delta; a missing platform section fails loudly — darwin and linux sections both reviewed and committed). RSS is one daemon-wide stream, not yet per individual workload. | S2 |
| PERF-12 | Baselines committed only after review; never written by the comparing run | DONE. `--bootstrap-baselines` removed from the PR and trusted edition8 lanes (comparing runs never write a baseline; a missing reviewed baseline fails loudly), the scale baseline is written only by the explicit `--write-baseline` flag (missing-file and corrupted-metric failure paths proven), and both per-platform baselines are now reviewed+committed: `tests/edition8/perf-baseline.json` gained the linux section measured on the CI execution class (linux/amd64 under Rosetta; qemu-emulated numbers rejected) with three consecutive green checks, and `scripts/edition8/scale-baseline.json` gained its linux section from a canonical `--turns 1000` run with a compare-mode proof. | S1 |

### 5.2 Agent quality evals (p.42) — DOM-95, S1
200 tasks (bug fixes, features, migrations, security, multi-repo, embedded
part changes), hidden tests, reference patch, one-fifth after training
cutoff. Each task 3× on ≥2 families (one hosted, one local Ollama), four
combinations (Faktor alone, +Tangerine, +Kiwi, both). Gates: false completion
≤1%, zero safety violations, verified resolve no drop beyond 95% one-sided
noise, honest disclosure 95%, consistency tracked, cost/verified task alert
at +20%. Lanes: 30-task nightly, full weekly + release candidate; new
`agent_eval` evidence gate beside `real_provider`. Current: **MISSING**;
offline certification never uses a model.

### 5.3 Revenue suites (pp.43–44) — DOM-96, S2
- Free core: full community suite with billing disabled/no licence gives
  identical results; Apache core without paid crates passes; dependency scan
  fails if core imports paid. Current: `deny.toml` requires every workspace
  member Apache-2.0; no split test.
- Plans/entitlements/billing: 4 plans × 12 features + unknown plan + foreign
  org (72 cells); entitlement change at three admission points; metering under
  crashes/retries; invoice reconciliation to micro-unit; licence signature,
  trial/expiry/offline grace/seats. Current: billing service and 12 value
  entitlements exist; suites missing.
- Kiwi: PR corpus ≥90% planted defects, ≤5% false alarms; evidence mandatory;
  webhook signatures/replays/one verification per (repo, head); metering
  within 1%, infra failures never billed; published conformance suite.
- Tangerine: C/C++ call graph precision/recall vs clang
  `compile_commands.json`; function-pointer calls reported unknown; partially
  indexed tree answers "partial"; published conformance suite. Prerequisite:
  tree-sitter currently Rust, Python, TypeScript, JavaScript only.
- Part-change: 25 real changes (sensor swaps/register maps, flash chips, I2C
  addresses, pin-compatible MCU variants, passives needing no code); reference
  patch + hidden tests; cross-builds; Renode; HIL on release. Traps:
  incompatible replacement refused with reason; altered datasheet fact caught;
  no-firmware-change part produces no patch. Cost equals sourcing quote
  difference; report validates schema.

### 5.4 Lanes and release (pp.45–46)
Already covered in `programme.md` §7. Findings:
- LANE-01 (S1): PR lane writes a visual baseline when a platform has none
  (`.woodpecker/untrusted/pr.yaml:537,550`), so a run can compare against a
  baseline it wrote itself (p.7, p.40).
- LANE-02 (S1): no CI lane runs `scripts/edition8/scale-probe.mjs` (p.3,
  p.41).
- LANE-03 (S2): no `agent_eval`, `live_contracts`, `assistive_tech`, `scale`
  or `hil` signed evidence gates (p.46).
- LANE-04 (S2): no quarantined-test owner/expiry enforcement (p.10 rule 5).
- LANE-05 (S2): mutation score per crate is not tracked; no 80% floor on
  authority-bearing crates (p.10 rule 7).
- LANE-06 (S2): no change-aware test selection; every PR currently runs a
  fixed step set (`.woodpecker/untrusted/pr.yaml`), so "only tests relevant
  to changed files" is not implemented.

### 5.5 Dogfood and visual evidence
- **DOG-01 — S2:** the isolated real-daemon dogfood is green (86/86 in
  `target/certification/edition8-summary.json`, 16 flows per host) but is not
  a registry row and is not wired to a release gate. The report's rule 1
  (p.10) says the registry extends `scripts/dogfood.mjs`; no per-surface
  dogfood row exists.
- **VIS-01 — S1:** PR visual bootstrap self-write (LANE-01).
- **VIS-02 — S2:** `apps/jetbrains/frontend/src/test/resources/parity/
  visual-baselines.json` lacks `windows`; `scripts/windows-visual-baseline.ps1`
  will refuse it.
- **VIS-03 — S2:** Linux baseline records are written by the pipeline, not a
  reviewed human artifact (p.9).

### 5.6 History hygiene
- **HYG-01 — S1:** core dumps (AG-19).
- **HYG-02 — S3:** `sessions.rs` grandfather entry (AG-20).
- **HYG-03 — S2:** `.gitignore` has no `*.core`; no blob content sniffing
  (ELF/minidump/Mach-O) and no size cap for non-allowlisted blobs (p.7).

---

## 6. Counts and severity roll-up

- **At-a-glance findings:** 20. Of these 18 are fixes/proof gaps, 1 is a new
  defect (AG-05), 2 are non-fixes (AG-19, AG-20).
- **Registry/coverage findings:** 11 (REG-01…REG-11).
- **Shared-harness findings:** 18 (HAR-01…HAR-18) — HAR-01/02/03/05/06/07/
  08/09/10/11/12/14/15/16/17/18 now CLOSED or scoped-closed (see the table
  in §3); HAR-04's fsync/rename half is complete and drift-enforced, with
  the SQLite-commit failpoint enumeration as its remaining delta; HAR-13
  was already DONE. The residual physical-rig halves of HAR-18 and the
  200-task corpus of HAR-17 are tracked in their rows.
- **Detailed domain rows:** 94 (DOM-01…DOM-94).
- **Performance/evals/revenue/lanes/dogfood/visual/hygiene:** 28
  (PERF-01…12, DOM-95…96, LANE-01…06, DOG-01, VIS-01…03, HYG-01…03).
- **Total named findings/proof gaps: 171.**
- **S0:** AG-01, REG-01, DOM-42, DOM-55, DOM-71. Everything else is
  S1–S3 as marked (DOM-05 and PERF-09 landed their 1M/plan proofs and
  moved to S1/DONE).
- **Act-now:** three items (AG-19, AG-05, AG-01), plus the report's
  build-order step 1 "1-micro cap regression" (AG-12). HAR-13/PERF-01
  are DONE (nightly scale lane with committed per-platform baselines and
  the superlinearity gate in PR+nightly).

Overlap is intentional: the At-a-glance rows are the HEAD-change view; the
DOM/PERF rows are the same obligations at surface granularity. A programme
row is not complete until the corresponding DOM/PERF proof exists at the
exact commit, even when the At-a-glance code fix is present.
