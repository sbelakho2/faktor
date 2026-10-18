# Implementation plan — test programme and HEAD fixes

Review HEAD: `ec502901cf1d0b0318114f6d88340fb67f99e71e` ("Fixes",
2026-10-18, 361 commits). Worktree at review time: uncommitted sibling WIP
plus untracked `Report.pdf` and these docs. Every step below names the file
targets, the evidence, and whether the code fix, the proof, or both are
missing. "DONE" means code + proof; "PARTIAL" means the code fix exists but
the report's proof does not; "MISSING" means neither.

Order is dependency order: history hygiene and the two live defects first,
then the registry/selector that every later step relies on, then per-surface
fixes, then performance and live/hardware lanes.

**Worktree note (2026-10-18):** the shared checkout contains uncommitted WIP
from sibling workers: settlement policy-row binding, the memory range
predicate + tests, and an untracked programme/selector under
`scripts/programme/` and `tests/programme/`. None of it is in HEAD or counts
as release evidence. Statuses below are HEAD unless explicitly marked
**WIP**. Audit the WIP against this plan before committing: in particular
`tests/programme/model.mjs` uses a different six-layer taxonomy than the
report diagram (see REG-11), and `tests/programme/selector.mjs` selects
crates/suites rather than per-surface rows.

## Phase 0 — Act now (the report's p.3 list + build-order step 1)

### P0.1 Remove the two core dumps from history; add hygiene gate
- **Report:** p.3 "Act now"; At-a-glance row 19 (p.7); build order step 1
  (p.46).
- **Current:** MISSING. `qemu_chrome_20261018-133409_41.core`
  (89,522,176 B) and `_43.core` (89,534,464 B) are tracked in `ec502901`;
  `.gitignore:15-32` has `core`/`core.*` but no `*.core`; no content sniff.
- **Steps:**
  1. Back up: tag and branch `backup/pre-core-purge` at `ec502901`.
  2. Rewrite history (exact commands in `act-now.md`).
  3. Add `*.core` and `*.core.*` to `.gitignore`.
  4. Add `scripts/check-blob-hygiene.mjs` + a `static-authority` test:
     sniff ELF core `7f 45 4c 46`, minidump `4d 44 4d 50`, Mach-O
     `fe ed fa ce`/`fe ed fa cf`/`ce fa ed fe`/`cf fa ed fe`, Java class
     `ca fe ba be`; reject blobs above a reviewed size cap unless
     allowlisted.
  5. Wire the gate into `.woodpecker/untrusted/pr.yaml` and
     `.woodpecker/trusted/trusted.yaml` after the existing static scans.
- **Files:** `.gitignore`, `scripts/check-blob-hygiene.mjs` (new),
  `tests/static-authority/src/lib.rs`, `.woodpecker/**`.
- **Evidence:** `git rev-list --objects --all | grep -E 'qemu_chrome.*\.core'`
  empty; hygiene test fails a seeded ELF/minidump/Mach-O fixture; CI green.
- **Done when:** fresh clone size drops by ~179 MB and the gate catches a
  seeded dump in a test branch.

### P0.2 Replace the memory `LIKE` predicate and prove the index plan
- **Report:** p.3 "Act now"; At-a-glance row 5 (p.4); detailed memory rows
  pp.19, 21.
- **Current:** NEW DEFECT at HEAD. `crates/store/src/ledger.rs:1655` and
  `crates/store/src/longtask.rs:404` still use
  `AND kind LIKE 'memory_v2:%'`; the count query at `longtask.rs:506` also
  uses `LIKE`. **WIP** in the shared worktree replaces all three with the
  half-open range and adds tests; not committed.
- **Steps:**
  1. Replace both with the half-open range
     `kind >= 'memory_v2:' AND kind < 'memory_v2;'` (bound parameter, no
     interpolation).
  2. Do the same in `memory_fact_count_v2` (`longtask.rs:500-515`).
  3. Do **not** add a new index: `memory_fact` already has
     `UNIQUE (session_id, kind, key)` (`schema.rs:107-115`), whose
     `sqlite_autoindex_memory_fact_1` is the exact `(session_id, kind, key)`
     index. The plan test must name that index; do not add a redundant v40
     migration. `workspace_memory_fact` has the analogous
     `(workspace_id, kind, key)` primary key.
  4. Add `EXPLAIN QUERY PLAN` assertions in
     `crates/store/src/query_plan_tests.rs` for both pagers and the count.
  5. Add a 200,000-row churn test in `crates/store/src/ledger_tests.rs` /
     `longtask_tests.rs`: page p50 and count under 1 ms; kinds
     `MEMORY_V2:x` and `memoryXv2:y` must not match; `memory_v2:decision`
     must match.
- **Files:** `crates/store/src/ledger.rs`, `crates/store/src/longtask.rs`,
  `crates/store/src/query_plan_tests.rs`, `crates/store/src/ledger_tests.rs`,
  `crates/store/src/longtask_tests.rs`.
- **Evidence:** plan names `sqlite_autoindex_memory_fact_1`; 200k timing;
  negative-kind corpus.
- **Done when:** `cargo test -p faktor-store` and the query-plan test pass;
  no `LIKE 'memory_v2` remains (`rg`).

### P0.3 Bind every settlement path and ban lazy `task_id()` reads
- **Report:** p.3 "Act now"; At-a-glance row 1 (p.3); detailed settlement
  pp.27–28; build order step 1 (p.46).
- **Current:** PARTIAL. `InSession` settlement carries an optional task id,
  but `settle_resolved_verifications` (`settlement.rs:243-247`),
  `recovery.rs:570` and the watcher path pass `None`; `settle_orchestrated`
  (`settlement.rs:911`) and `settle_resolved_verifications` (`:283`) read
  `handle.task_id()` lazily. The report counts 18 remaining lazy reads.
  **WIP** in the shared worktree writes `task_id` into `RunPolicyRow` and
  `DurablePlanRow` and removes the current-task fallback (`run_policy_task_id`
  refuses legacy rows); the 18 lazy reads are still present and uncommitted.
- **Steps:**
  1. Write the durable run linkage row at admission for every run path,
     including queued prompt, recovery and shadow watcher.
  2. Make `RunSettlement::InSession` require a `TaskId` (not `Option`) and
     pass it through `settle_run`, `settle_orchestrated`,
     `settle_resolved_verifications`, the watcher and recovery.
  3. Replace every settlement-adjacent `handle.task_id()` with the bound id.
     Target modules: `crates/agent/src/runtime/settlement.rs`,
     `crates/agent/src/runtime/turn/drive.rs`,
     `crates/agent/src/runtime/turn/state.rs`,
     `crates/agent/src/runtime/compaction.rs`,
     `crates/agent/src/runtime/retrieval.rs`,
     `crates/agent/src/runtime/verification_attribution.rs`,
     `crates/agent/src/outcome_ledger.rs`,
     `crates/orchestrator/src/task_executor/settlement.rs`,
     `recovery.rs`, `integration.rs`,
     `crates/orchestrator/src/runtime.rs`,
     `crates/orchestrator/src/runtime_helpers.rs`.
  4. Add a static-authority scan banning `handle.task_id()` /
     `handle.task_id` in the settlement files above, with an exact-line
     allowlist only for the admission-time read that creates the binding.
  5. Add a deterministic interleaving harness (`tests/interleaving` or
     `crates/agent/src/runtime/interleaving_tests.rs`): pause at every named
     await point of turn N's settlement; start prompt N+1; assert task N+1's
     row is byte-identical and not terminal before its first provider call.
  6. Add a crash-and-recover variant: kill after the linkage write, before
     settlement; the recovered settlement uses the recorded id.
- **Files:** as above + `tests/static-authority/src/lib.rs` +
  `crates/orchestrator/src/task_executor/settlement_tests.rs` +
  `crates/agent/src/runtime/settlement_tests.rs`.
- **Evidence:** interleaving matrix (every await point), task/run row diff,
  static scan output.
- **Done when:** no lazy read remains and the 18-read count in the report is
  zero.

### P0.4 Schedule the scale probe in the nightly lane
- **Report:** p.3 "Act now"; p.41 workload table; pp.45–46 lanes.
- **Current:** PARTIAL. `scripts/edition8/scale-probe.mjs` exists; no
  `.woodpecker/**` reference (rg returns none).
- **Steps:**
  1. Add a `scale-probe` step to `.woodpecker/trusted/nightly.yaml` (after
     `longrun`, before `certificate-nightly`), with its own target volume.
  2. Build the daemon/CLI prerequisites the script expects, then run
     `node scripts/edition8/scale-probe.mjs --turns 1000` with
     `FAKTOR_CHROME` unset (no browser needed).
  3. Emit a lane marker:
     `scripts/certification/lane-marker.sh write --lane scale --status passed
     --artifact target/certification/edition8/scale.raw.json --out
     target/certification/lanes/scale.json`.
  4. Make `certificate-nightly` require the `scale` marker and artifact.
  5. Add the lane to `tests/lanes.toml` and to the release checklist.
  6. Keep all thresholds from the script: turn curve ≤20/≤2, p95 ≤15 s;
     transcript page p95 ≤500 ms, curve ≤6, search ratio ≤25; history curve
     ≤6; memory/child/claims/MCP/skills/hooks/16-stream/1M-churn gates.
- **Files:** `.woodpecker/trusted/nightly.yaml`, `tests/lanes.toml`,
  `scripts/certification/evidence.mjs` (lane list), release docs.
- **Evidence:** `target/certification/lanes/scale.json` signed; curves in
  `target/certification/edition8/scale.raw.json`.
- **Done when:** the probe runs on the nightly cron and a deliberately
  injected super-linear query fails the lane.

### P0.5 Add the 1-micro cap regression
- **Report:** build order step 1 (p.46), detailed budget p.30.
- **Current:** PARTIAL. Caps are passed into prompt/task
  (`prompt.rs:68-70`, `task.rs:710-744`); `task.rs:731` documents zero as
  absent; no plain-prompt 1-micro/1-token test.
- **Steps:** add a test in `crates/server/src/api/router_task_tests.rs`:
  submit a prompt with `max_cost_micro = 1` and `max_tokens = 1`; assert
  zero provider calls, typed budget refusal, and no task completion. Add a
  zero-cap test asserting the documented "0 = no cap" rule (or change the
  rule and document it).
- **Done when:** both tests fail when the cap check is removed.

## Phase 1 — Make the registry truthful

### P1.1 Adopt `faktor-surface-coverage/v1` rows
- **Files:** `tests/invariant-coverage.json` (schema + rows),
  `scripts/check-invariant-coverage.mjs`, `scripts/discover-capabilities.mjs`
  (unchanged output), docs.
- **WIP:** `tests/programme/discovery.mjs` already enumerates the 17 Rev8
  routes and 4 credential routes; `tests/programme/model.mjs` already bans
  delegation. These are untracked; audit and integrate rather than
  reimplement, but reconcile the six-layer taxonomy (REG-11).
- **Steps:**
  1. Add the fields from `programme.md` §1.1 (`class`, `promise`, `owns`,
     `tests`, `lane`, `platforms`, `adversarial`, `evidence`).
  2. Ban `delegate` on rows whose class is a product surface; keep the five
     invariants as separate rows.
  3. Require every row to name at least one test function and one lane.
  4. Generate the 359 rows from discovery; add the 17 missing routes with
     real tests; delete the 4 stale rows.
- **Done when:** `node scripts/discover-capabilities.mjs --json` count = 359;
  registry surface count = 359; checker `--check` OK.

### P1.2 Generate failing stubs per surface × mandatory case
- **New:** `scripts/surface-stubs.mjs`; stub modules per class.
- **Rules:** one stub per missing row/case; stub name is stable; the PR gate
  runs selected stubs; replacing a stub with a real test deletes it in the
  same commit.
- **Done when:** deleting a test makes its row red; a new route without a
  row fails.

### P1.3 Derive control status and counts
- **Files:** `docs/specs/edition8/controls.json`,
  `scripts/edition8/control-exercise.mjs` or a new
  `scripts/certification/derive-controls.mjs`.
- **Steps:** derive `exists/partial/missing` from executable control rows;
  stop hand-writing `counts.by_status` (currently 25/160/15 at
  `controls.json:39` while `backlog.md:7` says 40/148/12); regenerate the
  175 contract gaps.
- **Done when:** the two summaries agree and CI rejects a typed status.

### P1.4 Make every surface row selected and evidenced
- **Files:** `tests/lanes.toml` (new), `scripts/certification/select-tests.mjs`
  (new), `scripts/certification/evidence.mjs`.
- **Steps:** row owns globs; lane manifest; evidence schema
  `faktor-row-evidence/v1`; signed lane marker.
- **Done when:** every selected test writes a row evidence file; certificate
  enumerates all 359.

## Phase 2 — Change-aware selector and lanes

### P2.1 Implement `select-tests.mjs`
- **Contract:** `programme.md` §5.1.
- **WIP:** `scripts/programme/select-tests.mjs` and
  `tests/programme/selector.mjs` already implement conservative
  crate/test-target selection with typed refusal for unmapped paths. They do
  not yet map to per-surface registry rows or emit row-level evidence; extend
  them rather than adding a second selector.
- **Unit tests:** fixtures for docs-only, one-crate change, shared-schema
  change, rename, unmapped file, deleted test, new surface.
- **Acceptance:** selector fails closed; selected set is deterministic;
  skipped rows carry a reason; PR never counts an unselected test.

### P2.2 Rewire the PR lane
- **File:** `.woodpecker/untrusted/pr.yaml`.
- **Steps:** replace the fixed Edition 5/8 step list with
  `select-tests.mjs --lane pull`; keep mandatory gates unconditional
  (fmt, registry, static scans, blob hygiene, ratchets, structural); remove
  `--bootstrap-baselines --allow-unreviewed-platform` from the PR lane
  (LANE-01); a missing platform baseline fails.
- **Done when:** a docs-only PR runs docs gates only; a `crates/store`
  change runs the store dependents + route smoke; an unmapped file fails.

### P2.3 Wire the remaining lanes
- **Files:** `.woodpecker/trusted/trusted.yaml`,
  `.woodpecker/trusted/nightly.yaml`, `tests/lanes.toml`.
- **Steps:** trusted = all rows for each OS; nightly = scale/fuzz/mutation/
  interleaving/soak/30-task eval; weekly = live contracts/canaries/Kiwi/
  screen readers/Renode; release = all + HIL + signed certificate.
- **Done when:** each lane emits `target/certification/lanes/<lane>.json` and
  the certificate requires it.

### P2.4 Remove self-written baselines and add evidence ownership
- **Files:** `apps/vscode/scripts/edition8-visual.mjs`,
  `scripts/edition8/run.mjs`, `.woodpecker/untrusted/pr.yaml`,
  `apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json`.
- **Steps:** baseline writes only in a dedicated reviewed job; add Windows
  JetBrains record; a compare run never writes.
- **Done when:** PR fails on a missing baseline; the baseline change includes
  rendered images and a reviewer.

## Phase 3 — Code and test fixes (surface order)

Status is the current HEAD. Every item includes the exact test that makes it
green.

| # | Finding | Status | Code target | Test target |
|---|---|---|---|---|
| 3.1 | Settlement identity / 18 lazy reads (AG-01) | PARTIAL | `crates/orchestrator/src/task_executor/settlement.rs`, `recovery.rs`, `integration.rs`; `crates/agent/src/runtime/{settlement,turn/drive,turn/state,compaction,retrieval,verification_attribution,outcome_ledger}.rs`; `crates/orchestrator/src/{runtime,runtime_helpers}.rs` | new interleaving harness + static-authority scan |
| 3.2 | Tool activation dead `BoundExhausted` (AG-02) | PARTIAL | `crates/agent/src/runtime/retrieval.rs:1761`; remove `ToolActivationScan::BoundExhausted` at `tool_loop.rs:435-447` | `retrieval_tests.rs`: 1,000,000 churn; corrupt value = `ReadFailed` |
| 3.3 | Child control hostile row space (AG-03) | PARTIAL | `crates/session/src/child.rs:61-66,606-643` | `child.rs` tests: 1,000,000 unrelated + 5,001 controls → typed refusal, no truncation |
| 3.4 | Ledger full sweep (AG-04) | PARTIAL | `crates/session/src/ledger/mod.rs:3712`; `manager.rs:1263` | `ledger/tests.rs`: corrupt inside verified prefix; scheduled sweep fails |
| 3.5 | Memory LIKE + index (AG-05) | NEW DEFECT / WIP | `crates/store/src/ledger.rs:1655`, `longtask.rs:404,506`; existing `UNIQUE(session_id, kind, key)` | `query_plan_tests.rs` + `ledger_tests.rs`: 200k, negative kinds |
| 3.6 | Workspace cache real watchers (AG-06) | PARTIAL | `crates/fs/src/workspace_service.rs:28,353-374` | `crates/fs/src/lib.rs`: 1,000 turns/OS, real watchers; in-use root not evicted |
| 3.7 | Routing 64-tail decision equality (AG-07) | PARTIAL | `crates/session/src/manager.rs:638`; `crates/store/src/billing.rs:1197` | `billing_tests.rs`: recorded sessions, tail decision == full |
| 3.8 | History cursor update semantics (AG-08) | PARTIAL | `crates/server/src/native/session.rs:1503`; `crates/store/src/sessions.rs:561`; `schema.rs` v39 | concurrent-update walk; plan test |
| 3.9 | Transcript search decoded/Unicode (AG-09) | PARTIAL | `crates/store/src/messages.rs:433-470`; `foundation_ext.rs:348-430` | reference visible-text oracle; Unicode corpus; JSON-key query |
| 3.10 | Proof totals boundaries (AG-10) | PARTIAL | `crates/server/src/native/proof.rs:810-930` | 0/1/499/500/501/10,000; broken chain |
| 3.11 | Route boundary fuzz + panic hook (AG-11) | PARTIAL | no code target; harness | `tests/route-fuzz` under panic hook, every route body |
| 3.12 | Budget cap 1/0 on plain prompts (AG-12) | PARTIAL | `crates/server/src/native/prompt.rs`, `task.rs:731` | `router_task_tests.rs`: 1-micro/1-token = zero calls; 0 rule |
| 3.13 | Task/run tag pair matrix (AG-13) | PARTIAL | `task.rs:21-76`; `session.rs:249-270` | exhaustive pair test + both IDE renderers |
| 3.14 | VS Code outcome/window edge (AG-14) | PARTIAL | `apps/vscode/media/chat.js` merge | `transcript-deny-check.mjs`: outcome × exit-code; window edge |
| 3.15 | Webview adversarial lifecycle (AG-15) | PARTIAL | `apps/vscode/src/webview.ts:43`; `edition8/panel.ts` | message-before-listener, hide/reveal, reload, 100 reveals |
| 3.16 | JetBrains real LAF scrollbars (AG-16) | PARTIAL | `UiWork.kt:804-818` | real-IDE audit with native LAF |
| 3.17 | Migration full schema matrix (AG-17) | PARTIAL | `migration/*` remaining `SCHEMA_HEAD - N` | every v0..head vs fresh head full schema compare |
| 3.18 | Skills cursor (AG-18) | PARTIAL | `foundation_ext.rs:1492-1515` | 600 skills all reachable; cursor stable under insert |
| 3.19 | Blob hygiene (AG-19/HYG-03) | MISSING | `.gitignore`, `scripts/check-blob-hygiene.mjs` | seeded ELF/minidump/Mach-O/size tests |
| 3.20 | Shrink-only exemption ratchet (AG-20) | MISSING | `tests/static-authority/src/lib.rs:7762-7778` | list length non-increasing; removal when file fits |
| 3.21 | Credential route tests (REG-04) | MISSING | `crates/server/src/native/foundation.rs:567-660` | planted-secret scan; rotate/delete; vault unavailable |
| 3.22 | CLI per-command template (REG-05) | MISSING | `crates/cli/**` | 43 commands incl. updater/enterprise/sandbox |
| 3.23 | Model transport differential (DOM-16) | PARTIAL | `crates/provider/**` | same scripted conversation per family; chaos proxy |
| 3.24 | Provider chaos/fake-live (DOM-17,23,78) | PARTIAL | adapters | hostile catalog/MCP/vendor corpus; fake vs live |
| 3.25 | Control status derivation (REG-06) | MISSING | `docs/specs/edition8/controls.json`, new script | derived 40/148/12; reject typed |
| 3.26 | Windows JetBrains baseline (REG-07) | MISSING | `visual-baselines.json` | Windows record reviewed; gate passes |
| 3.27 | Screen-reader certification (REG-08) | MISSING | `tests/edition8/workflows.mjs` | NVDA/VoiceOver/Orca transcripts |
| 3.28 | Dogfood rows (DOG-01) | PARTIAL | `scripts/dogfood.mjs` | 86/86 + per-flow registry rows |
| 3.29 | Invariant mutation floor (LANE-05) | PARTIAL | `tests/invariants.toml` | 80% mutation on authority crates |

## Phase 4 — Performance work

### P4.1 Nightly scale lane
P0.4 above. Store curves and `RSS` per workload; a missing baseline is a
failure, not an auto-write.

### P4.2 Memory query plans and timing
P0.2. Add 200,000-row timing; p50 <1 ms; plan names the existing
`sqlite_autoindex_memory_fact_1` `(session_id, kind, key)` index; count query
same. Do not add a redundant index.

### P4.3 1,000,000-row churn
Add to `crates/agent/src/runtime/retrieval_tests.rs` and
`crates/session/src/child.rs` tests. Assertions:
- tool activation lookup returns the same value at 0 and 1,000,000 churn
  rows; p95 flat;
- child control read returns the same queue at 5,001 controls and
  1,000,000 unrelated facts; typed refusal at the bound, never truncation;
- memory page result and timing unchanged; no full scan in the plan.

### P4.4 1,000 shadowed turns with real watchers
`crates/fs/src/lib.rs` per OS: no 1 ms deadline; count threads/handles before
and after; durable root still open; eviction only idle roots.

### P4.5 History 2,000/20,000 and concurrent updates
`crates/store/src/sessions.rs` + route tests: every conversation once;
updated-mid-walk rule asserted; curve ≤6.

### P4.6 100,000-message transcript
`crates/session/src/messages.rs`, `foundation_ext.rs`: every seq once; page
p95 ≤500 ms; curve ≤6; search 10k→100k ratio ≤25; decoded Unicode.

### P4.7 1,000,000-entry ledger open
`crates/session/src/ledger/mod.rs`: open after incremental verification is
bounded; full sweep scheduled; memory ceiling.

### P4.8 Resource census
Add thread/file-handle/child/RSS counters to the harness; run after every
long suite; 1,000 start/stop cycles return to baseline.

### P4.9 Agent evals
Build `tests/agent-eval` (200 tasks, hidden tests, reference patch, 3 runs,
≥2 model families, 4 combinations); 30-task nightly; full weekly/release.
Gate: false completion ≤1%, zero safety violations, verified resolve no drop
beyond 95% one-sided noise, honest disclosure 95%, cost alert +20%.

### P4.10 Revenue/product
Add free-core (billing off/no licence), plan × feature (4×12+unknown+foreign
= 49), Kiwi PR corpus, Tangerine C/C++ conformance, part-change 25 tasks
with traps. Gate at release.

### P4.11 Hardware
Add Renode corpus weekly and HIL release evidence for hardware-touching
changes.

## Phase 5 — Verification and release

Run exactly (AGENTS.md and CI):

```
cargo fmt --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-ci
node scripts/discover-capabilities.mjs --json
node scripts/check-invariant-coverage.mjs --check
node scripts/check-invariant-coverage.mjs --release
node scripts/check-ignored-tests.mjs
node scripts/certification/select-tests.mjs --base <base> --head <head> --lane pull --out target/certification/selection.json
node scripts/edition8/scale-probe.mjs --turns 1000
```

Acceptance:
- `doctor` exits 0 on a fresh data dir and a corrupted store is reported
  typed.
- Registry: 359 surface rows, 0 delegated, 17 fixed, 4 deleted.
- `--release` has no gaps and no stale rows.
- Selector: docs-only, one-crate, shared-schema, rename, unmapped and
  deleted-test fixtures behave as specified.
- Every row has an evidence file at the exact commit; certificate verifies
  all lane markers.
- Hygiene: no core dump in `git rev-list --objects --all`; seeded dump
  fixture fails.
- Perf: p.41 budgets pass; injected super-linear query fails nightly.
- Eval: 200-task gate meets p.42 thresholds; live/assistive/HIL manifests
  signed.

## Status summary for the 20 At-a-glance items

| Item | Status | Missing proof |
|---|---|---|
| 1 Settlement | PARTIAL | no-task paths; 18 lazy reads; interleaving; static ban |
| 2 Tool activation | PARTIAL | 1M churn; dead state removal |
| 3 Child control | PARTIAL | 5,001 controls + 1M unrelated; typed refusal |
| 4 Ledger verify | PARTIAL | corrupt verified prefix; scheduled full sweep |
| 5 Memory paging | NEW DEFECT / WIP | range predicate; plan; 200k timing; negative kinds |
| 6 Workspace cache | PARTIAL | real watchers; in-use root eviction |
| 7 Routing tail | PARTIAL | tail-vs-full decision equality |
| 8 History paging | PARTIAL | mid-walk update semantics; plan; 20k |
| 9 Transcript search | PARTIAL | decoded-text oracle; Unicode folding |
| 10 Proof totals | PARTIAL | 0/1/499/500/501/10k; broken chain |
| 11 Tiny compaction | PARTIAL | route-wide boundary fuzz + panic hook |
| 12 Budget caps | PARTIAL | 1-micro/1-token; zero-cap rule |
| 13 State tags | PARTIAL | exhaustive pair matrix in both IDEs |
| 14 Outcome merge | PARTIAL | outcome × exit-code matrix; window edge |
| 15 Webview lifecycle | PARTIAL | message-before-listener/reveal/reload/100 reveals |
| 16 JetBrains scrollbars | PARTIAL | real LAF, not Metal headless |
| 17 Migrations | PARTIAL | full schema compare for every v0..head |
| 18 Skills | PARTIAL | cursor; 600 reachable |
| 19 Core dumps | MISSING | purge history; `*.core`; content sniff |
| 20 File-size exemption | MISSING | shrink-only ratchet test |
