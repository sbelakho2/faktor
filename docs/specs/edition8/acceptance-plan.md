# Edition 8 acceptance and verification plan

This is an executable plan, not an implementation. It turns
`acceptance.md`, `visual.md` and `ci-dogfood.md` into ordered work with file
targets. The PDF supplies no CI, dogfood, SLO or certificate schema; every
item marked **Proposed** is a repository-side decision, not a spec value.

## 0. Inputs and frozen counts

Read-only inputs (owned by the sibling extraction):

- `docs/specs/edition8/screens.md` — 72 screen contracts, S01–S72.
- `docs/specs/edition8/controls.json` — 200 control records.
- `docs/specs/edition8/flows.md` — W01–W16 narratives and screen links.
- `docs/specs/edition8/build-map.md` — control → screen → backend mapping.

Frozen by this plan:

| Artifact | Count |
|---|---:|
| Screens | 72 |
| Host contracts | 144 |
| Controls | 200 |
| Acceptance scenarios A01–A40 | 40 |
| Workflows W01–W16 | 16 |
| Workflow cells (5 per host) | 160 |
| Reference vectors (72 × 4 themes) | 288 |
| Base host renderings (72 × 2 hosts × 2 primary themes) | 288 |
| High-contrast renderings (72 × 2 hosts) | 144 |
| Maximal alternate (4 themes per host) | 576 |
| Platform records (VS Code/JB × win/mac/linux) | 6 |
| Optical measurements (≥30 buttons × 2 hosts × themes) | ≥60 per theme set |

No derived count may replace a printed count. The registry gate fails on
missing, extra, duplicate or renamed IDs.

The sibling `screens.md:901-903` records that 40 Rev8 surfaces (S33–S72) have no
dedicated implementation yet. This plan verifies evidence; it does not build
those surfaces. A missing screen must fail `edition8-registry`, not be marked
`not_certified` and allowed through.

## 1. Phase 0 — registry and contract freeze

**Goal:** one machine-readable registry that every harness reads; no harness
hard-codes 32/200/40 again.

**Files:**

- `docs/specs/edition8/registry.json` — proposed schema
  `faktor-edition8-registry/v1`: screens (id, title, group, page, surfaces,
  primary action, control IDs), controls (200), acceptance (A01–A40),
  workflows (W01–W16, five cells), host-surface map, theme map.
- `docs/specs/edition8/expectations.json` — numeric/textual fixtures from
  `acceptance.md` §6 and 3.1 (240/320/480/420/720+, 200%, 50%, 5.00→5000000,
  18,400+4,096+2,048+8,224=32,768, 3/250/5,000/0.2, ≤500, 8/128/512/32×512,
  ≥30 buttons, 1,036 labels, 80 pairs).
- `docs/specs/edition8/source-lock.json` — PDF page/line provenance and a
  hash of `/tmp/faktor-ui3.txt`; do not treat `/tmp` as a release artifact.

**Files to generate/read:**

- `apps/vscode/src/generated/edition8Registry.ts`
- `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt`

**Command (proposed):**

```bash
node scripts/edition8/codegen.mjs \
  --screens docs/specs/edition8/screens.md \
  --controls docs/specs/edition8/controls.json \
  --acceptance docs/specs/edition8/registry.json \
  --out-vscode apps/vscode/src/generated/edition8Registry.ts \
  --out-jetbrains apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt
node tests/edition8/registry.mjs
```

**Gate `edition8-registry`:** 72 unique screens S01–S72; 200 unique controls
in 15 letter groups; 40 acceptance IDs exactly A01–A40; 16 workflow IDs
exactly W01–W16; 144 host contracts; every screen has ≥1 control and a
contract page; every control maps to ≥1 screen; every workflow maps to printed
screens; no extra IDs. Exit 1 on drift.

**Stop if:** the sibling's screens/controls files do not yet expose the 72/200
counts; do not guess IDs.

## 2. Phase 1 — host adapters and the click-every-control harness

**Goal:** every one of the 200 controls is reachable, named, validated,
applied, fenced and recoverable in both hosts.

**VS Code:**

- `apps/vscode/scripts/edition8-control-exercise.mjs` — drives the
  `edition8Registry` through the real webview/command surface for each control.
  For each control record:
  - default value/state; label; accessible name/role/state;
  - validation: invalid, empty-required, range, locked/inherited;
  - timing: staged → pending → acknowledged; no optimistic success;
  - atomic apply: one patch, revision conflict;
  - ownership fence: old session/worktree/daemon handle refuses;
  - recovery: typed error and a working recovery action;
  - forbidden fake state scan (`Saved`, `Connected`, `Verified`, `0%`, `$0`,
    `Isolated`, `Remembered`, `Deleted`, `Stopped`, `Idle` without receipt).
- `apps/vscode/scripts/edition8-dead-control.mjs` — reverse map: no orphan
  handler, no control without handler, no screen without a render.
- Target file: `apps/vscode/src/edition8/{registry,bindings,view,html,commands}.ts`.

**JetBrains:**

- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8ControlExercise.kt`
  — same 200 controls through real Swing panels; `AccessibleContext` names,
  roles, states; validation/apply/fence/recovery.
- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8DeadControl.kt`.
- Wire into `apps/jetbrains/build.gradle.kts` smokeMains and
  `apps/jetbrains/compile-and-smoke.sh` (known smokes list).

**Counts:** 200 controls × 2 hosts = 400 control-host runs; 0 missing
handlers, 0 orphan handlers, 0 dead controls. The harness must click/invoke
every control at least once and record `exercised: 200`.

**Gate `edition8-controls-{vscode,jetbrains}`:** `{controls:200, exercised:200,
failed:0, missing_handlers:0}`.

## 3. Phase 2 — shared scenario runner A01–A40

**Goal:** the 40 printed acceptance scenarios execute from one shared
observation model, with host adapters only for mechanics.

**Files:**

- `tests/edition8/acceptance.mjs` — shared runner; scenario table built from
  `acceptance.md` §3 and `registry.json`; validates IDs A01–A40 exactly; fails
  on duplicates/extras/missing.
- `apps/vscode/src/edition8/acceptanceHost.ts` — VS Code adapter: open screen,
  set field, click, read visible text/role/state, capture receipt.
- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8AcceptanceHost.kt`
  — JetBrains adapter with the same operations.
- `tests/edition8/evidence.mjs` — schema checks, count checks, forbidden-state
  scan, source-page binding.
- `tests/edition8/fixtures/a01…a40/` — only fixtures that the document's
  contracts print; no invented product states.

**Observation record (proposed `faktor-edition8-observation/v1`):**

```json
{
  "id": "A30",
  "host": "vscode",
  "preconditions": ["S21 open", "spending limit field empty"],
  "steps": ["enter 5.00", "submit", "read stored value"],
  "visible": ["Spending limit per run", "5.00", "USD"],
  "field_checks": {"micro_usd": 5000000, "tokens_display": "120,000"},
  "forbidden": ["optimistic Saved", "floating-point cap change"],
  "source": "PDF p.343",
  "passed": false,
  "reason": "not run"
}
```

**Gate `edition8-acceptance-{vscode,jetbrains}`:** `{scenarios:40, passed:40,
failed:0}`; every ID has `preconditions`, `steps`, `visible`, `field_checks`,
`forbidden`, `source`; no missing/extra; no `passed:true` without a host
receipt.

**Do not** treat `A01` and `W01` as the same; the PDF prints no mapping.

## 4. Phase 3 — workflow runner W01–W16 (five cells per host)

**Goal:** all 16 workflows, pointer and keyboard, AT, compact/zoom and
recovery, in both hosts.

**Files:**

- `tests/edition8/workflows.mjs` — reads the 16 rows from `acceptance.md` §4
  (`flows.md` for narrative), emits five cells per host: `pointer`, `keyboard`,
  `screen_reader`, `compact_zoom`, `recovery`.
- `apps/vscode/src/edition8/workflowHost.ts`,
  `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8WorkflowHost.kt`
  — adapters.
- `tests/edition8/fixtures/w01…w16/` — fixtures matching the printed steps;
  no fabricated features. If a step depends on a capability the product does
  not have, the cell is `not_certified` with the exact reason, never `passed`.

**Execution:** 16 workflows × 2 hosts × 2 input modes = 64 runs; each run
records 5 cells (160 cells). A workflow passes only when every cell passes in
both hosts (3.1). The screen-reader cell records every announcement; the
compact cell runs 240 and 320 px at 200%; recovery replays the printed
recovery action.

**Gate `edition8-workflows-{vscode,jetbrains}`:** `{workflows:16, passed:16,
cells:80, failed:0}` per host (5 cells × 16). No cell may be blank or
`skipped` without a typed `not_certified` reason.

**Printed ID conflict:** 3.1's parenthetical "(W06 approval, W11 recovery,
W12 history)" conflicts with 3.2. Follow the flow names and record the
mismatch in the gate document.

## 5. Phase 4 — visual contracts and real-host visual matrix

**Goal:** every screen in both hosts, every required theme/width/zoom/state, with
a digest and a reviewable capture.

### 5.1 Structural contract gate

- `tests/edition8/visual-contracts.mjs` — 72 screens × 2 hosts = 144 contracts;
  each carries width/theme/state/text-scale/keyboard/IME/stream fields and a
  per-contract digest. Fail on drift, missing screen, wrong surface, or a
  contract with no host renderer.
- `tests/edition8/platforms.mjs` — six records (vscode/jetbrains ×
  linux/macos/windows); a missing record is `not_certified`, never inherited.
  Extend the existing `scripts/check-visual-platforms.mjs` policy to VS Code
  and themes.

**Gate `edition8-visual-contracts`:** `{screens:72, hosts:2, contracts:144,
failed:0}`; platform list is exactly the six required records.

### 5.2 VS Code real-host matrix

- `apps/vscode/scripts/edition8-visual.mjs` — launch the real Extension Host
  (not Chromium-only), open each screen with fixed fixtures, capture:
  - widths 240, 320, 420, 480, 720+;
  - themes Dark Modern, Light Modern, High Contrast Dark, High Contrast Light;
  - zoom 100/150/200%; OS text scaling where supported;
  - locales English/German/Arabic (RTL);
  - states first-read/empty/ready/stale/error/pending/acknowledged;
  - native surfaces: Settings editor, Quick Pick, modal, `vscode.diff`,
    terminal, simple browser.
- Store under `target/certification/edition8/visual/vscode/<platform>/<screen>/<theme>/<width>/<zoom>/<state>.png`
  with a sidecar JSON (build, OS, font fingerprint, fixture, sha256).
- `apps/vscode/scripts/verify-vsix.mjs` already proves package/IDE load; keep
  it as a separate gate, not a visual substitute.

### 5.3 JetBrains real-host matrix

- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8VisualMatrix.kt`
  — real IDE window or the existing offscreen panel renderer, pinned fonts;
  widths 240/320/480/800; zoom 100/125/200; themes Light/Dark/High Contrast;
  native surfaces: Configurable Settings, DialogWrapper, popup chooser,
  DiffManager, console, JCEF.
- Extend `JetBrainsHostMatrixSmoke.kt` rather than fork it; keep ZIP-hosted
  proof and source provenance.
- Pin platform records in
  `apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json`;
  do not inherit from `visual-baselines.json`.

**Gate `edition8-visual-platforms`:** six certified records; 0 `not_certified`
for a release claim; each record has font fingerprint, host build, OS build,
theme, width, zoom, state, sha256.

### 5.4 Optical and contrast

- `tests/edition8/optical.mjs` — rasterise each host capture at 3×; crop
  interior; measure cap-top/baseline; report median/mean/5th/95th/max; require
  ≥30 buttons per host and theme; fail if median moves >0.5 px from the R8
  reference or any label uses a fixed text offset.
- `tests/edition8/contrast.mjs` — compute WCAG 2.2 AA from rendered tokens;
  4.5:1 normal, 3:1 large/graphics/boundaries; report the nine host defaults;
  Faktor-drawn elements must use ≥3:1; HC separately.
- `tests/edition8/copy-retention.mjs` — 1,497 segments/5,973 strings/838
  changes: every R7 string is either present or has a recorded destination;
  no silent deletion.

**Gates:** `edition8-optical` (≥30 per host/theme), `edition8-contrast`
(0 unresolved Faktor-drawn failures), `edition8-copy-retention` (0
unaccounted segments).

## 6. Phase 5 — assistive-technology protocol

**Goal:** 3.1's AT condition, not an AccessibleContext smoke.

**Files:**

- `tests/edition8/assistive.mjs` — driver/checker for recorded AT sessions.
- `tests/edition8/assistive/{nvda,windows,voiceover,macos,orca,linux}.json` —
  session records: host build, AT version, flow, announcement transcript,
  focus path, pass/fail, evidence clip/sha256.
- `scripts/edition8/at-session.md` — manual protocol script (NVDA on Windows,
  VoiceOver on macOS, Orca on Ubuntu for VS Code); the plan itself is not a
  substitute for a human session.

**Checks:** every state change announced once; no token-by-token streaming;
approval announced without stealing focus; focus visible/not trapped/not under
sticky content; disappearing item goes to nearest stable sibling/heading;
closing detail returns to exact trigger; IME never submits.

**Gate `edition8-assistive`:** W07/W14/W16 (approval/recovery/history) and the
compact A39 flow have records on each required AT; 0 unannounced state
changes; 0 focus traps. A missing AT record is `not_certified`.

## 7. Phase 6 — performance runs

**Goal:** enforce the qualitative A40 rules with a measurable, baseline-
anchored bound; do not invent a spec SLO.

**Files:**

- `apps/vscode/scripts/edition8-stream-probe.mjs` — extend
  `stream-probe.mjs` with a `budget` object and platform baseline. Workloads:
  500 messages + 10k tokens + 200 deltas/s; 10k burst; 1k rows; scrolled-away
  and pinned; unloaded-history Find; 60 s stream; 2k permission/agent rows.
- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8PerfHarness.kt`
  — same workloads on the real EDT path; reuse `EdtWorkHarness` assertions.
- `tests/edition8/perf.mjs` — baseline reader/gate; 3 runs, median; writes
  `perf.raw.json`.
- `apps/vscode/scripts/baselines/edition8/<platform>.json` and
  `apps/jetbrains/frontend/src/test/resources/parity/edition8-perf-baselines.json`.

**Proposed budgets** (from `ci-dogfood.md` §4.3, not PDF):

- p95 delta-to-visible and keystroke-to-paint ≤1.25× committed platform
  baseline;
- 0 dropped/duplicated/reordered events; 0 full transcript rebuilds per delta;
  queue/transcript caps hold;
- transcript 500→1,000 messages ≤2× time; 0 O(n²) signature;
- Find over unloaded history exact and ≤1.25× baseline p95;
- bounded terminal/log buffer; RSS plateau ≤5% after warmup over 60 s;
- 10→200 control patch ≤6× time.

A missing baseline is `not_certified`; a budget miss fails the gate. An
absolute p95 number must be adopted by the product owner; the PDF gives none.

**Gate `edition8-perf`:** `{workloads:11, failed:0, baseline_present:true}` per
host, with per-platform records.

## 8. Phase 7 — isolated dogfood

**Goal:** exercise real workflows against the real daemon in a disposable
worktree and data dir, with no network.

**Files:**

- `scripts/edition8/dogfood.sh` — model on
  `scripts/edition5/dogfood.sh`:
  1. build real `faktor-cli`;
  2. `git worktree add --detach HEAD`;
  3. temp data dir/config;
  4. loopback-only fake OpenAI-compatible provider;
  5. real daemon `/native/ready`;
  6. run VS Code and JetBrains W01–W16 flows;
  7. write `target/certification/edition8/dogfood.raw.json` with
     `isolated:true`, `network:"loopback-only"`;
  8. cleanup trap removes worktree and temp dir.
- `apps/vscode/scripts/edition8-dogfood.mjs` — VS Code host flows.
- `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition8Dogfood.kt`
  — JetBrains flows against the real daemon.
- Reuse `scripts/edition5/fake-openai.mjs` only if it remains protocol-compatible;
  otherwise `scripts/edition8/fake-openai.mjs`.

**Checks:** dirty main checkout is ignored; no network egress; all 16 workflows
reach the printed recovery point; no credentials in config; evidence counts
`flows:16, passed:16, failed:0` per host. This is not a soak/load/perf lane.

**Gate `edition8-dogfood`:** isolated + loopback-only + 16/16 both hosts.

## 9. Phase 8 — CI wiring and gating

**Files:**

- `.woodpecker/pr.yaml` — add `edition8` lane after `jetbrains-smoke`; run
  `node scripts/edition8/run.mjs` (not `--no-verify`); write lane marker.
- `.woodpecker/pr.yaml` `vscode-render` — retain Chromium check; add real
  Extension Host visual lane or move to trusted/nightly.
- `.woodpecker/trusted/trusted.yaml` — real VS Code Extension Host + real
  JetBrains IDE visual lanes; `scripts/edition8/dogfood.sh`; evidence verify.
- `.woodpecker/nightly.yaml` — full visual matrix, AT, performance, all six
  platform records; no PR-time soak.
- `scripts/edition8/run.mjs` — orchestrates gates in order; refuses partial.
- `scripts/dogfood.mjs` — register the Edition-8 plan gates/lanes so coverage
  law sees them.
- `scripts/certification/evidence.mjs` — add Edition-8 gate kinds and count
  checks (or a sibling `tests/edition8/evidence.mjs` invoked by it).

**PR policy:** evidence is verified in the PR, not only trusted. A stale,
dirty, missing or wrong-count document fails. Lane markers must bind commit,
tree, runner OS/arch, commands digest, artifact sha256.

**Gate `edition8-certificate`:** all gate docs current for the same commit;
all six platform records present or the claim is blocked; all counts match.

## 10. Phase 9 — certificate and release claim

**Outputs:**

```
target/certification/edition8-summary.json
target/certification/edition8/*.raw.json
target/certification/lanes/edition8.json
target/certification/ci-certification.json
target/certification/manifest.json
```

**Certificate contents (proposed):**

- schema `faktor-edition8-certificate/v1`;
- commit/tree/dirty/generated_at;
- counts: 72 screens, 144 contracts, 200 controls, 40 acceptance, 16
  workflows, 160 cells, 6 platform records;
- per-host results; per-platform visual/perf/AT status;
- explicit `not_certified` list with reasons (never silently inherited);
- artifact sha256 for every raw doc and screenshot set;
- forbidden-fake-state scan result;
- performance baselines and budget verdicts;
- dogfood isolation/network facts.

**Release claim fails if:**

1. any of 72/144/200/40/16 is missing or duplicated;
2. any A/W cell is `passed` without a host receipt;
3. any screen/platform/theme/AT record is missing and the claim says
   "certified";
4. any forbidden fake state is observed;
5. any performance baseline is absent or over budget;
6. dogfood is not isolated/loopback-only;
7. the PR verifier cannot bind the evidence to the exact commit.

## 11. Ordered execution summary

| Order | Phase | Gate | Primary target |
|---:|---|---|---|
| 1 | Registry freeze | `edition8-registry` | `docs/specs/edition8/registry.json`, generated registries |
| 2 | Click every control | `edition8-controls-*` | `apps/vscode/scripts/edition8-control-exercise.mjs`, `Edition8ControlExercise.kt` |
| 3 | A01–A40 | `edition8-acceptance-*` | `tests/edition8/acceptance.mjs` + host adapters |
| 4 | W01–W16 | `edition8-workflows-*` | `tests/edition8/workflows.mjs` |
| 5 | Visual contracts | `edition8-visual-contracts` | `tests/edition8/visual-contracts.mjs` |
| 6 | Real-host visual matrix | `edition8-visual-platforms` | `edition8-visual.mjs`, `Edition8VisualMatrix.kt` |
| 7 | Optical + contrast + copy | `edition8-optical`, `edition8-contrast`, `edition8-copy-retention` | `tests/edition8/{optical,contrast,copy-retention}.mjs` |
| 8 | AT protocol | `edition8-assistive` | `tests/edition8/assistive.mjs` + session records |
| 9 | Performance | `edition8-perf` | `edition8-stream-probe.mjs`, `Edition8PerfHarness.kt` |
| 10 | Dogfood | `edition8-dogfood` | `scripts/edition8/dogfood.sh` |
| 11 | CI + certificate | `edition8-certificate` | `.woodpecker/*`, `scripts/edition8/run.mjs`, evidence |

**Why this order:** the registry and control handlers are the cheapest failures;
A/W prove behaviour before pixels; real-host visual comes before optical/
contrast because those measurements need captures; AT/performance/dogfood
depend on stable host adapters; the certificate is last and verifies
everything against one commit.

## 12. Definition of done

- 72/72 screens, 144/144 host contracts, 200/200 controls in both hosts.
- 40/40 A scenarios and 16/16 W workflows (all five cells) in both hosts.
- Six platform records certified (or the release claim explicitly blocked).
- Four host themes plus high contrast; 240/320/420/480/720+; 200%; German and
  Arabic; no clipping outside intended scroll regions.
- ≥30 buttons/host/theme measured; 1,036 labels accounted; 80 contrast pairs
  resolved or reported.
- AT records for NVDA/VoiceOver/Orca; announcements once; no focus traps.
- Performance baselines present; proposed budgets pass; no O(n²)/unbounded
  path.
- Isolated loopback-only dogfood 16/16 in both hosts.
- Evidence verified in the PR, bound to commit/tree, with explicit
  `not_certified` reasons and zero forbidden fake states.
