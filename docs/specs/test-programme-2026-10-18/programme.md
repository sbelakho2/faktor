# Faktor test programme — implementation contract

Source: `Report.pdf` pp.2–17, 39–47. Review HEAD `ec502901`. This document
turns the report's programme into a buildable specification. It does not
replace `tests/invariants.toml`, `scripts/check-invariant-coverage.mjs` or
`scripts/dogfood.mjs`; it extends them (p.10, rule 1).

Core rule: **a promise ships only when its tests pass in every lane at one
commit** (p.2). A row with no passing test at that commit turns the build
red. No row is satisfied by a generic invariant.

## 1. Surface inventory: 359 rows

The report's "359 discovered surfaces" (p.2, p.7) decomposes exactly as:

| Surface class | Count | Discovery source |
|---|---:|---|
| Native daemon routes | 246 | `crates/server/src/api/lifecycle.rs` route block |
| CLI commands | 43 | clap command tree |
| IDE contributions | 56 | 55 VS Code manifest contributions + 1 JetBrains action; VS Code has 55, not 58 (3 stale config rows) |
| Built-in tools | 8 | tool registry |
| Provider kinds | 6 | provider profiles |
| **Total** | **359** | `node scripts/discover-capabilities.mjs --json` |

Current `tests/invariant-coverage.json` has 346 `discovered` rows, not 359:
the 17 missing Revision 8 routes below, offset by 4 stale rows. A correct
registry has exactly 359 surface rows after adding 17 and deleting 4.

`node scripts/check-invariant-coverage.mjs --check` must print `OK` only when
the discovered set and the registry set are equal. Today it fails:

```
check-invariant-coverage: undiscovered-capability: native.post.native.catalog.probe ...
...
check-invariant-coverage: FAIL (17 violation(s))
```

### 1.1 Per-surface proof template (no generic-invariant delegation)

Every surface row is a `faktor-surface-coverage/v1` record:

```json
{
  "id": "native.post.native.session.id.prompt",
  "class": "native-route",
  "promise": "start one turn; duplicate returns the same run; no side effect on refusal",
  "owns": ["crates/server/src/native/prompt.rs", "crates/server/src/native/prompt_tests.rs"],
  "tests": [
    {"path": "crates/server/src/native/prompt_tests.rs", "name": "prompt_duplicate_submission_is_idempotent"}
  ],
  "lane": "pull",
  "platforms": ["linux", "macos", "windows"],
  "adversarial": ["duplicate", "strict-body", "auth", "bounds", "crash-write", "ide-parse"],
  "evidence": "target/certification/pull/native.post.native.session.id.prompt.json"
}
```

Rules:
- `tests` must name at least one real test function; `delegate` is forbidden
  for a surface row (p.11, rule 8). The five generic invariants stay as
  additional rows; they cannot make a surface row green.
- `lane` is the cheapest lane that must run the row; release re-runs every
  row at the release commit.
- `platforms` is explicit per row; no "all" shortcut.
- `adversarial` must include at least the mandatory cases for its class.
- A row without a test is `gap` and blocks release; a stub that fails blocks
  release (p.16, surface stub generator).
- Status is derived from the evidence file, never typed (p.11, rule 9).

Mandatory cases per class:

- **Native route (246):** auth missing/wrong token; strict body (unknown and
  missing fields); typed error shape/status; path/ID bounds and traversal;
  duplicate/idempotent submission; illegal state transition; cross-session
  isolation; crash between durable writes; client parse of the exact body;
  rate/body size bound; no secret in response.
- **CLI command (43):** `--help` exit 0; unknown flag exit 2; missing daemon
  typed failure; stdout schema parsed; stderr redaction; no-args behaviour;
  two identical runs (idempotent or exactly-once effect); `doctor
  --data-dir` exit code; `updater`, `enterprise`, `sandbox` named cases.
- **IDE contribution (56):** command registered; argument schema; webview
  message handled; control action dispatches the declared route; no orphan
  result; host parity (VS Code vs JetBrains); disabled/unauthorized state.
- **Tool (8):** allowed/denied; timeout; cancel; output cap; path escape;
  malformed args; shell metacharacters; child cleanup.
- **Provider kind (6):** fake/live same normalized events; error mapping;
  retry/backoff; locality; cost/usage; capability selection with no
  `provider == "..."` branch.

Every row must be discovered automatically. A new route, CLI command, IDE
contribution, tool or provider with no row fails PR (the current
`undiscovered-capability` check already does this for routes).

### 1.2 The 17 Revision 8 routes that need rows

These are the exact `undiscovered-capability` IDs. Each row needs an own
behaviour test, not a delegation to `INV-NATIVE-ROUTE-BEHAVIOR`.

| # | Route | Required own proof |
|---|---|---|
| 1 | `POST /native/catalog/probe` | absent connection is typed; hostile endpoint (timeout, 10,000 models, malformed payload) never starts a paid call; probe result is bounded |
| 2 | `GET /native/presets/{id}/roles` | role list is stable; unknown preset 404; no cross-workspace read |
| 3 | `POST /native/presets/{id}/roles` | role override is durable; duplicate update idempotent; unknown role typed |
| 4 | `GET /native/presets/{id}/fallback` | fallback chain order; cycle refused; deleted model surfaces typed |
| 5 | `POST /native/presets/{id}/fallback` | cycle/deleted model refused before write; update applies next run |
| 6 | `GET /native/mcp/servers/{id}/auth` | auth state truthful; expired/revoked token; no secret in body |
| 7 | `POST /native/mcp/servers/{id}/auth` | double connect idempotent; bad credentials typed; no token in logs/events |
| 8 | `GET /native/session/{id}/review/comments` | list order; cross-session 404; survives restore/fork per contract |
| 9 | `POST /native/session/{id}/review/comments` | create once; malformed body refused; author/time derived |
| 10 | `PATCH /native/session/{id}/review/comments/{comment}` | update own only; stale revision; deleted comment typed |
| 11 | `DELETE /native/session/{id}/review/comments/{comment}` | delete own only; idempotent delete; no orphan in export |
| 12 | `POST /native/session/{id}/changes/accept` | accepts exactly the previewed tree; stale preview refused; no partial landing |
| 13 | `POST /native/session/{id}/checkpoints/{sequence}/restore/preview` | preview has no effects; exact file/hash manifest; invalid sequence typed |
| 14 | `DELETE /native/session/{id}` | running turn stops; tombstone written; 404 after; no cross-session deletion |
| 15 | `POST /native/session/{id}/export` | round-trip import; planted secret absent; size cap; running turn snapshot truthful |
| 16 | `GET /native/session/{id}/settings` | effective layer + provenance; unknown session 404 |
| 17 | `PUT /native/session/{id}/settings` | per-session override next turn; stale revision conflict; reset clears |

Registration sites are `crates/server/src/api/lifecycle.rs:1022-1126`;
handlers are `crates/server/src/native/r8.rs` and
`foundation_ext.rs`; current tests are in
`foundation_rev8_contracts_tests.rs` but are not registry rows.

Delete the 4 stale rows: `cli.help`,
`vscode.configuration.faktor.auto-start`,
`vscode.configuration.properties`, `vscode.configuration.title`.

### 1.3 The other surface gaps the rows must expose

- **35 routes named by no test** (p.7), including credential store, rotate and
  delete: `GET/POST /native/providers/credentials`,
  `POST /native/providers/credentials/rotate`,
  `DELETE /native/providers/credentials/{handle}`. The credential row must
  run a planted-secret scan over response, log, export, event frame and
  crash dump (p.32).
- **19 CLI subcommands run by no test** (pp.8,35,36): 8 `updater`, 10
  `enterprise`, 1 `sandbox`.
- **6 skill routes named by no test** (p.34) plus the missing cursor:
  `GET /native/skills` must prove all 600 discovered skills are reachable;
  the current `.take(500)` with no cursor is not reachable beyond 500
  (`crates/server/src/native/foundation_ext.rs:1492-1515`).
- **Metadata routes** (p.30) and **session settings/review/restore** routes
  (pp.29–33) must be named by their own rows.

## 2. The full promise registry

The registry is not only 359 surface rows. The report's sources diagram
(p.2) and coverage table (pp.7–9) add:

| Layer | Rows | Required proof |
|---|---:|---|
| Product surfaces | 359 | per class above |
| Controls | 200 | one contract test per control record, derived status |
| Screens | 72 | structural contract + visual matrix in both hosts |
| Workflows | 16 | five cells per IDE: pointer, keyboard, screen reader, compact/zoom, recovery |
| Acceptance scenarios | 40 | A01–A40 executable, forbidden-state scans, field checks, one adversarial variant |
| Invariants | 158 | unit + wiring + planted mutation; five shared invariants remain additional rows |
| **Core total** | **845** | before revenue/product rows |

Revenue and product rows (pp.43–44):
- 4 plans × 12 value features + unknown plan + foreign org = 49 plan cells;
- Free core: billing-disabled/no-licence run, Apache core split, dependency
  scan;
- Kiwi PR corpus, evidence, webhook, metering, conformance;
- Tangerine C/C++ call graph, partial index, conformance;
- Part-change 25-task corpus, traps, sourcing cost.

One row per promise, naming tests, lane and platforms (p.10). The row is
the unit of selection, evidence and release gating.

## 3. The six layers (p.2 diagram; pp.10–17)

The report's page-2 diagram defines exactly these six layers. They are
cumulative: a surface promise normally needs L1 + L2 + L3, and the
domain-specific layer(s).

> Note: the untracked WIP `tests/programme/model.mjs` uses a different
> six-layer taxonomy (`structural`, `unit`, `property`, `fault`,
> `integration`, `live`). That taxonomy folds Agent evals and Hardware into
> `live` and adds `structural` as a layer. The report diagram is
> authoritative; reconcile the WIP before landing (see `findings.md`
> REG-11). Structural gates remain PR gates, not a proof layer.

### L1 — Unit and property
- **Definition:** exact values and generated inputs.
- **Proves:** arithmetic, parsing, serialization, money/budget units, pure
  state transitions, algorithmic properties.
- **Execution:** every PR; property tests use fixed stored seeds on PR and
  random seeds nightly; shrinking is mandatory (p.16).
- **Evidence:** test name, seed/corpus hash, exact commit, platform.
- **Gate:** any failure blocks merge.

### L2 — State model and crash
- **Definition:** kill at every durable write.
- **Proves:** journal/ledger/store/CAS/queue/crash recovery; replay is
  deterministic; no half-state; no double effect; no lost completion.
- **Execution:** PR smoke on the changed state machine; trusted push full
  fault campaigns; nightly kill-at-every-write campaigns and interleavings.
- **Evidence:** failpoint id/offset, before/after digests, replay log,
  recovered state hash.
- **Gate:** a crash point that loses an acknowledged effect, or replays a
  different state, blocks release.

### L3 — Contracts
- **Definition:** the same test on fake and live.
- **Proves:** provider adapters, vendor APIs, SCM/GitHub, MCP, LSP/ACP,
  control plane; fake and live normalize to the same typed events.
- **Execution:** fake on every PR; live on weekly and release with own
  keys/accounts (p.46).
- **Evidence:** same test id against fake and live, normalized event diff,
  redacted request/response hashes.
- **Gate:** any contract that passes only on the fake is not green.

### L4 — IDE and screen readers
- **Definition:** real IDEs and spoken output.
- **Proves:** VS Code Extension Host and JetBrains real IDE (two newest
  versions, real look-and-feel), controls, screens, workflows, accessibility
  names/roles and spoken output.
- **Execution:** trusted push structural gates; weekly real IDE + NVDA,
  VoiceOver, Orca; release real IDE matrix.
- **Evidence:** host/version/OS, screen/workflow id, DOM/component tree hash,
  accessibility tree, spoken-output transcript.
- **Gate:** a screen that renders only in the harness, or a workflow that
  fails with a real screen reader, blocks release.

### L5 — Agent evals
- **Definition:** 200 tasks with hidden tests.
- **Proves:** the agent actually completes work; false completion, safety,
  verified resolve, honest disclosure, consistency, cost per verified task.
- **Execution:** 30-task smoke nightly; 200 tasks × 3 runs × ≥2 model
  families weekly and at release; four combinations (alone, +Tangerine,
  +Kiwi, both).
- **Evidence:** task id, model/family, run, hidden-test result, reference
  patch diff, cost, disclosure verdict.
- **Gate:** false completion ≤1%, zero safety violations, verified resolve
  no drop beyond 95% one-sided noise, honest disclosure 95%, cost alert at
  +20% (p.42).

### L6 — Hardware
- **Definition:** Renode simulation and HIL rig.
- **Proves:** embedded workflows, cross-builds, flash/debug, part-change
  effects.
- **Execution:** Renode corpus weekly; HIL at release and for
  hardware-touching changes.
- **Evidence:** board/rig id, firmware image hash, serial/ITM trace, test
  result, physical rig attestation.
- **Gate:** a hardware promise with no Renode/HIL evidence is not green.

## 4. How each of the 359 surfaces gets its own proof

Do not create 359 bespoke hand-written suites. Create one row per surface
and a template per class (pp.11–15). The template supplies the mandatory
cases; the row supplies the surface-specific promise and at least one named
test. A row can reuse harness helpers, fixtures and the fake daemon; it
cannot reuse another surface's assertion as its proof.

Implementation:
1. `scripts/surface-stubs.mjs` reads discovery + registry and emits a failing
   stub per missing row × mandatory case. Stubs land in the surface's own
   test module (for example `crates/server/src/native/route_stubs.rs`).
2. The PR gate runs `scripts/check-invariant-coverage.mjs --check` and then
   the selected stubs. A stub still failing is a red row (p.16).
3. Replace a stub with a real test; delete the stub in the same commit. The
   evidence file names the real function.
4. The registry records `owns` paths so change-aware selection can map a
   changed file back to rows.
5. A new surface discovered by `discover-capabilities.mjs` with no row fails
   PR immediately.

This is the only way to satisfy rule 8 (p.11): "No delegated coverage. A
surface counts as covered only by a test of its own behaviour."

## 5. Change-aware test selection (new requirement)

Goal: on a pull request, run only the tests relevant to changed files, plus
the mandatory gates; on trusted/nightly/weekly/release lanes, run the full
lane set. "Relevant" is computed from the registry, not guessed from
directory names.

### 5.1 Selector contract

New script: `scripts/certification/select-tests.mjs`.

```
node scripts/certification/select-tests.mjs \
  --base "$CI_COMMIT_BEFORE_SHA" \
  --head "$CI_COMMIT_SHA" \
  --lane pull \
  --out target/certification/selection.json
```

Inputs: git diff (`--name-status`, rename-aware), `tests/invariant-coverage.json`
(`owns`/`tests`), `cargo metadata` dependency graph, lane manifest.

Algorithm:
1. Compute changed files. A rename selects both old and new owner rows.
2. Map each changed file to rows whose `owns` glob matches. A changed test
   file selects its own row; a changed fixture selects every row that names
   it.
3. Expand crate dependency closure: if a changed crate is a library, add
   every workspace crate that depends on it (reverse `cargo metadata`).
4. Mandatory gates always run: `cargo fmt --check`, registry/discovery
   equality, static-authority scans, blob-hygiene sniff, edition structural
   gates, and (for server changes) the route fuzz smoke.
5. If any changed file maps to no row, fail with `unmapped-change`; an
   unmapped change is never skipped.
6. For shared authorities (`crates/store/src/schema.rs`, migrations,
   protocol, `faktor-core`), select the full dependent closure and the
   registry gate; a schema change cannot run a narrow subset.
7. Docs-only changes run docs sync/branding/registry checks and no Rust
   tests.
8. Emit a deterministic shell command file plus JSON:
   `{base, head, changed[], selected_rows[], selected_tests[],
   skipped_rows[{id,reason}], commands[]}`.
9. Fail closed if selection is empty while changed files are non-docs.
10. Cache compiled artifacts by toolchain + base, never cache test results.

Execution rules:
- **Pull request:** selector output only; no unselected test may be counted
  as evidence.
- **Trusted push:** all rows whose `platforms` include the runner OS; full
  fault campaigns.
- **Nightly/weekly/release:** full lane set; selector only orders work, it
  never reduces coverage.
- A skipped row's reason is recorded; release re-runs every row regardless
  of earlier skips.

### 5.2 Lane manifests

Add `tests/lanes.toml` mapping lane → required row classes, commands,
platforms, time budget, evidence kind. The selector, CI and certificate all
read the same file. A lane-specific test with no manifest entry fails the
lane (no orphan tests).

## 6. The five lanes (pp.45–46)

### Lane 1 — Pull request: every PR
- **Runs on:** Linux; no secrets; forks included.
- **Suites:** format, lint, unit, property (fixed seeds), static scans,
  fault smoke, requirement-registry + surface-stub gate, query-plan tests,
  history-hygiene content sniffing, source-size ratchets, short route fuzz
  under a panic hook, Edition 8 structural gates.
- **Change-aware:** only selected rows + mandatory gates.
- **Evidence:** selection JSON, test results, registry diff, ratchet report.
- **Gate:** blocks merge.

### Lane 2 — Trusted push: three OSes
- **Runs on:** main pushes on Linux, macOS, Windows.
- **Suites:** full workspace, full fault campaigns, route + CLI templates
  for all surfaces, real-IDE UI, install matrix.
- **Change-aware:** no; all rows whose platforms include the OS.
- **Evidence:** per-OS result matrix, process/thread/handle census.
- **Gate:** blocks release candidates.

### Lane 3 — Nightly: scale and fuzz
- **Runs on:** schedule (nightly, `0 3 * * *` UTC in
  `.woodpecker/trusted/nightly.yaml`).
- **Suites:** fault campaigns at scale, fuzzing, mutation sampling,
  concurrency/interleaving, `scripts/edition8/scale-probe.mjs`, 1,000,000-row
  churn, resource census, soak, 30-task agent-eval smoke.
- **Change-aware:** no.
- **Evidence:** scale curves, mutation score, fuzz corpus hash, resource
  census, eval summary.
- **Gate:** blocks next morning's merges until triaged. A flaky result is
  quarantined with owner and expiry; quarantine is not green.

### Lane 4 — Weekly live: real services
- **Runs on:** schedule, own keys/accounts, isolated data.
- **Suites:** provider contracts, vendor canaries, GitHub App/SCM, Kiwi PR
  corpus, full agent eval, NVDA/VoiceOver/Orca, Renode corpus.
- **Change-aware:** no.
- **Evidence:** `live_contracts`, `agent_eval`, `assistive_tech`,
  `sourcing_canaries`; redacted.
- **Gate:** blocks sales of the affected product, not just the release.

### Lane 5 — Release: one commit
- **Runs on:** tag.
- **Suites:** everything above at one commit; hardware-in-the-loop; signed
  evidence manifests.
- **Change-aware:** no; every row green.
- **Evidence:** release certificate listing every row + lane + platform +
  signed artifact hash.
- **Gate:** blocks shipping.

### 6.1 Release criteria (p.46, exact)

1. Every registry row green at the exact commit and platform, or a signed
   waiver with owner and expiry.
2. No flaky/quarantined test counted green.
3. Every control status derived, not typed.
4. Safety suite at least 20,000 cases, zero violations.
5. 1,000-turn perf curve, 100,000-message transcript and 2,000-conversation
   history within the p.41 budgets.
6. `agent_eval`, `live_contracts`, `assistive_tech` and `hil` evidence
   manifests signed and present; merge to main before evidence.
7. No merge, rebase or cherry-pick after evidence; the certificate names the
   exact commit.

## 7. Adversarial expectations

Every row's `adversarial` list is executable. The programme requires these
classes (pp.10–17, 19–27):

### Races
- Pause at every named await point and run every ordering of two drives.
- Settlement vs prompt N+1: task N+1 row must be byte-identical and must not
  become terminal before its first provider call (AG-01).
- Start/stop, writer queues, admission leases, concurrent settlement,
  concurrent migration, two daemons on one repo, owner edit during landing.
- Expected: one durable winner, typed loser, no lost effect, no duplicate
  side effect, no task/run identity fold.

### Ordering
- Out-of-order/duplicate event ids; old epoch after new; late tool result
  after window cut; message before listener; hidden/revealed webview; SSE
  reconnect at every id; reordered delta frames.
- Expected: idempotent application, no orphan UI row, no state regression,
  stream resumes exactly once.

### Corruption
- Flip bytes inside the verified ledger prefix (not only after it); corrupt
  SQLite pages, CAS blobs, JSON values, event tails; truncate streams;
  split UTF-8; malformed provider frames; 0/1/2-token compaction.
- Expected: loud typed failure, no silent truncation, no false verified
  status, recovery from the last good point where specified.

### Hostile input
- Symlink/junction traversal per OS; `/proc` and env injection; 1 GB tool
  output; 10,000 models; 10 MB diagnostics; 50 secret formats; prompt
  injection on every input channel; archive traversal; NaN/inf; `i64::MAX`;
  Unicode case-folding; IDN/IP/userinfo URL tricks; DNS rebinding.
- Expected: typed refusal, no panic, no side effect, bounded memory/context,
  redaction, security telemetry.

### Oracles
- Expected values are derived from a reference model, not copied from the
  implementation.
- Every test names exact values; a planted mutation must make it fail
  (p.10, rule 7).
- A test that passes on the fake but not the live contract is not green
  (L3).

## 8. Widening coverage

1. **Stub every surface** with the mandatory cases above; a failing stub is
   a red row.
2. **Mutation per crate:** planted mutations per invariant and per authority
   crate; 80% mutation score on authority-bearing crates; the score is a
   release gate (p.10).
3. **Property + model-based:** fixed seeds on PR, random on nightly, with
   shrinking and stored counterexamples.
4. **Fuzz:** route bodies, SSE/frame parsers, destination policy, archives,
   URLs, JSON, CLI args; corpus and crash artifacts are checked in.
5. **Differential:** fake vs live; old plugin N-1/N-2 vs current daemon;
   full history vs 64-call routing tail; decoded text vs raw JSON.
6. **Resource census:** thread/file-handle/child-process/RSS counts before
   and after every long suite; 1,000 start/stop cycles.
7. **Soak:** 10,000 turns with compaction; 1,000 shadowed turns per OS;
   1,000,000 fact churn.
8. **Coverage ratchet:** a new surface with no row, a deleted test with a
   green row, or a row with no evidence fails PR.

## 9. Evidence and gating model

- **One evidence file per row per lane per platform:**
  `target/certification/<lane>/<row-id>.json`, schema
  `faktor-row-evidence/v1`:
  `row, commit, platform, lane, test_path, test_name, cases[], result,
  duration_ms, artifact_hash, seed/corpus, generated_at`.
- **Signed lane marker:** `scripts/certification/lane-marker.sh` signs the
  lane summary; the certificate verifies the allowlisted key
  (`.woodpecker/trusted/trusted.yaml:1136`).
- **No self-certification:** a run cannot write its own baseline or
  evidence for a row it did not execute. The PR visual bootstrap must be
  removed (LANE-01).
- **Derived status:** controls, coverage counts and baselines are generated
  from code + results on every run (p.11, rule 9).
- **Flaky policy:** a flaky test is quarantined with owner and expiry; it is
  not green and cannot satisfy a row.
- **Release certificate:** every row ± waiver, all lane summaries, signed
  artifact hashes, perf curves, eval summary, visual baseline review.

## 10. Dogfood, visual, performance and hygiene gates

- **Dogfood:** preserve the isolated real-daemon run (currently 86/86 in
  `target/certification/edition8-summary.json`, 16 flows per host) and add
  a registry row per flow; do not let it become a substitute for surface
  rows.
- **Visual:** baselines are reviewed artifacts. PR never writes one; missing
  platform baseline fails. Add the Windows JetBrains record to
  `visual-baselines.json`; Linux records come from a reviewed run, not the
  comparing pipeline.
- **Performance:** add a nightly `scale` lane running
  `scripts/edition8/scale-probe.mjs`; store curves and RSS; gate exactly the
  p.41 budgets. The probe's own thresholds:
  turn curve ≤20 overall / ≤2 steady / p95 ≤15 s; transcript page p95
  ≤500 ms and curve ≤6; search 10k→100k ratio ≤25; history 2,000 curve ≤6;
  memory 5,000; child 500; claims 10,000; MCP/skills/hooks 500; 16 streams;
  1,000,000 churn; 1,000 shadowed turns.
- **History hygiene:** add `*.core` to `.gitignore`; sniff blob content for
  ELF core (`7f 45 4c 46`), minidump (`4d 44 4d 50`), Mach-O
  (`fe ed fa ce`/`fe ed fa cf`/`ce fa ed fe`/`cf fa ed fe`) and Java class
  (`ca fe ba be`); cap non-allowlisted blob size; run in PR and trusted
  lanes.

## 11. Definition of programme complete

- 359/359 surface rows have own tests; 0 delegated.
- 200/200 controls derived; 72/72 screens; 16/16 workflows × 5 cells;
  40/40 acceptance + adversarial variants; 158/158 invariants with mutation.
- 17 Rev8 routes and 4 stale rows resolved.
- Every row has lane/platform/evidence; release certificate generated from
  one commit.
- PR selector runs only relevant tests, with mandatory gates and no
  unmapped change.
- Nightly scale lane, weekly live lane, release HIL lane all wired.
- Core dumps removed from history; `*.core` and content sniffing enforced.
