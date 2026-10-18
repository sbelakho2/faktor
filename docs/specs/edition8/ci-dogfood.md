# Edition 8 CI, dogfooding, harnesses and performance

Source: `Faktor_IDE_UI_UX_3.pdf` (Revision 8, 17 October 2026), extracted to
`/tmp/faktor-ui3.txt`. This file separates three things that are easy to
conflate:

1. **What the PDF actually requires** (validation, evidence, release
   readiness, visual review).
2. **What this repository already runs** (file:line).
3. **What is a repository-side proposal**, not a spec value (CI lanes,
   dogfood isolation, numeric performance bounds, certificate schema).

## 0. The blunt finding

The PDF contains **no CI requirement, no dogfood program, no pipeline, no
latency/frame-rate/throughput SLO, no coverage percentage, no benchmark
harness, and no release-certificate schema**. Word counts in the extracted
text: `dogfood` 0, `continuous integration` 0, `pipeline` 0, `latency` 0,
`SLO` 0 (the only hits are substrings such as "slots"), `release criteria` 0.
It does say "do not set invented performance certifications" (p. 224).

Therefore:

- There are **no printed CI/dogfood expectations to “comply” with**.
- The binding delivery expectations are the **validation protocol** (3.1), the
  **workflow test records** (3.2), the **release evidence / definition of
  ready** (4.1–4.3), and the **acceptance scenarios** (4.8).
- Any numeric performance bound in this file is explicitly marked
  **Proposed (not in PDF)**. Do not cite it as a spec value.

## 1. What the PDF does require

### 1.1 Validation protocol (3.1, p. 182)

Status: **"defined, not yet executed."**

- Run each of the 16 workflows end to end in both hosts, once pointer and once
  keyboard-only.
- Repeat approval (W06), recovery (W11) and history (W12) flows with a screen
  reader and record every announcement.
- At 200% zoom and 320 px, confirm every primary, Deny and scoped Stop remains
  reachable without horizontal scrolling.
- Measure button-label centring in the real hosts with the 3.5 method on at
  least 30 buttons per host and theme.
- Measure contrast of the rendered UI in each theme; resolve the host-token
  findings in 1.11.
- Record results in per-workflow tables; a workflow passes only when every
  cell passes in both hosts.
- Pass criteria: task completed without help and without data loss; every state
  change visible and announced once; nothing announced only by colour; focus
  visible, never trapped, never hidden behind sticky content; recovery works
  and the draft survives every failure; copy is understood.
- Participants: at least five developers per host who use an agent weekly, plus
  at least two screen-reader users; sessions recorded; findings logged against
  screen and control IDs.

> **Printed ID conflict:** 3.1 says "Repeat the approval (W06), recovery (W11)
> and history (W12) flows". In 3.2, W06 is compaction, W11 is delegation and
> W12 is queue/background; approval is W07, recovery is W16, history is W14.
> Follow the named flow and record the mismatch in the evidence. Do not
> silently remap IDs.

Numeric/environment requirements: VS Code Stable current at test time on
Windows 11, macOS and Ubuntu; JetBrains IntelliJ IDEA plus one other IDE on the
same platform version; themes Dark Modern, Light Modern, High Contrast Dark and
Light; JB Light, Dark, High Contrast; sidebar widths 240/320/480; secondary
sidebar; panel; tool window right/bottom/floating/320; zoom 100/150/200%; NVDA,
VoiceOver, Orca; English/German/Arabic; 50% text expansion.

### 1.2 Workflow test records (3.2, pp. 183–186)

16 workflows, each with a five-cell record per host: **Pointer, Keyboard,
Screen reader, 200% / 320 px, Recovery**. All cells are blank in the PDF. Each
workflow prints screens, steps, recovery and "Complete when". See
`acceptance.md` §4.

### 1.3 Release evidence and readiness (4.1–4.3)

- Implementation waves 1–4: expose existing capability; configuration/model
  control; context/customization; advanced recovery/scale. "Avoid adding inert
  placeholder features" (p. 225).
- **Definition of ready:** "Both hosts pass the same behavioral fixtures for
  the delivered slice. Required controls have keyboard access, help, defaults,
  validation, source, apply timing, unavailable/loading/error/stale states and
  safe recovery. No fabricated usage or unsupported setting can appear active.
  Publish only claims backed by actual product tests." (p. 225)
- **Visual review gate:** "Review every screen with empty/large/invalid/stale
  data; 240/320/480 px tool windows; 200% text; long names; keyboard/IME; both
  host themes/high contrast; and running stream. Capture reference snapshots at
  fixed fixtures; reject overlap, hidden actions, off-grid helper text or
  mismatched state before merge." (p. 237)
- **Response/lifecycle:** page data and coalesce paint; keep durable event
  application separate from render scheduling; preserve manual scroll/focus;
  never rerender the whole form and erase in-progress input (p. 234).
- **Compact surface:** at 240–319 px primary Send/Stop and attention remain
  visible; 320–479 px chips wrap; ≥480 px full summary; no horizontal sidebar
  scroll trap (p. 224).

### 1.4 Delivery claims (4.6, p. 332)

- Each new control is labelled proposed unless actually present.
- Current-source facts are pinned; prior material is preserved with a dated
  addendum.
- Generated/prototype IDE scenes are not described as runtime screenshots or
  certification.
- Manual mobile hardware testing is not reintroduced.

### 1.5 What is already claimed as done (3.6, p. 196)

| Check | Method | Result | Status |
|---|---|---|---|
| Content retention | Every R7 string matched/relocated; word diff | 1,497 segments; 5,973 strings; 838 changes | Done |
| Optical centring | Ink cap-centre on rasterised labels | R7 −4.6 px; R8 +0.0 px median over 1,036 labels | Done (reference renderer) |
| Overflow/collisions | DOM measurement, four themes | No clipped content outside intended scroll | Done (reference renderer) |
| Contrast | WCAG formula on plate tokens | 80 pairs; 9 host defaults below threshold | Done; decisions open |
| Native conventions | VS Code/IntelliJ guidelines | Applied | Specified |
| Task flows in both IDEs | 3.1/3.2 | **Not run** | Not done |
| Assistive technology | NVDA/VoiceOver/Orca | **Not run** | Not done |
| Real fonts, zoom, HiDPI | In-host measurement | **Not run** | Not done |

The PDF explicitly says plates are "evidence about the design, not about the
shipped product" (p. 196) and "not a product screenshot and not evidence of IDE
behaviour" (p. 8).

### 1.6 Evidence limits (4.9, p. 346)

"141 selected source files were retrieved and Git-blob hash verified. The
review focused on IDE declarations, settings, core config and native protocol
boundaries; it is not a complete security audit or execution test of all code.
Later commits may change this baseline."

The section header says "141 REFERENCES"; the evidence limit says "141 selected
source files were retrieved and Git-blob hash verified". The F/B/R4/U list plus
the Revision 8 guidance set is a source inventory, not a coverage percentage.

## 2. Current repository harness map

### 2.1 Canonical gates and local certificate

| Repo surface | What it does | Evidence |
|---|---|---|
| `.woodpecker/pr.yaml` `linux` lane | `cargo fmt --check`, `check --workspace`, `test --workspace`, clippy `-D warnings` | PR lane marker `target/certification/lanes/linux.json` |
| `.woodpecker/pr.yaml` `vscode` lane (l. 233) | `npm ci`, build, `selftest.mjs`, `vsce package`, `verify-vsix` (`--min-media-files 4`, extract, packaged selftest, IDE load), branding scan | VSIX + lane marker |
| `.woodpecker/pr.yaml` `vscode-render` (l. 304) | Real headless Chromium `render-webview.mjs --check`; fail-closed Chrome discovery | Lane marker, no screenshot artifact |
| `.woodpecker/pr.yaml` `jetbrains-build` / `jetbrains-smoke` | Gradle build; Backend/NativeBridge/Frontend/Parity; ZIP host matrix; ControlExercise/Acceptance (Edition 5) | `target/certification/jetbrains-host-matrix.json`, raw JSON |
| `.woodpecker/pr.yaml` `edition5` (l. 487) | `node scripts/edition5/run.mjs --no-verify`; lane marker consumes `edition5-summary.json` | `target/certification/edition5-summary.json` |
| `.woodpecker/pr.yaml` `certificate` (l. 503) | ignored-test inventory, invariants, dogfood coverage, authority checks, CI-marker verification, CI certificate | `target/certification/ci-certification.json` |
| `.woodpecker/trusted/trusted.yaml` darwin-check (l. 617) | `cargo check`; VS Code Edition-5 control/acceptance exercise when Node exists | `darwin-check.json` |
| `.woodpecker/trusted/trusted.yaml` dogfood (l. 904) | `scripts/edition5/dogfood.sh` with `SKIP_JETBRAINS=1`; `run.mjs --no-verify`; `tests/edition5/evidence.mjs verify --allow-dirty` | `target/certification/edition5/dogfood.raw.json` |
| `.woodpecker/nightly.yaml` `ui-smoke-vscode` (l. 146) | Headless Chromium render + selftest | `ui-smoke-vscode.json` |
| `.woodpecker/nightly.yaml` `ui-smoke-jetbrains` (l. 224) | Packaged-plugin ZIP host matrix (`:frontend:smokeHostMatrixZip`) | `target/certification/jetbrains-host-matrix.json` |
| `scripts/certify-local.sh` | Full local/offline certificate; profiles fast/full; records skips honestly | `target/certification/manifest.json` |
| `scripts/dogfood.sh` | Sections: cargo gates, checkers/selftests, mutations, doctor, VS Code, JetBrains, Edition-5 gates, coverage law | `target/certification/manifest.json` |
| `scripts/dogfood.mjs` | Coverage law: every repo surface has a registered gate; CI lane coverage | `--coverage` red on an ungated surface |

### 2.2 Edition-5 gate (the closest existing analogue)

`tests/edition5/registry.mjs:28-32` hard-codes:

```
controls: 200
screens: 32
acceptance: 40
```

`scripts/edition5/run.mjs` enforces:

- VS Code controls `200/200`, failed 0, missing handlers 0 (l. 75).
- JetBrains controls `200/200` (l. 85).
- VS Code acceptance `40/40`, failed 0 (l. 95).
- JetBrains acceptance `40/40` (l. 105).
- Visual contracts `screens 32`, `hosts 2`, `contracts 64`, failed 0 (l. 115).
- Settings contracts failed 0 (l. 125).
- Perf delta `workloads 4`, failed 0 (l. 134).
- Dead controls: `dead 0`, missing handlers 0, orphan handlers 0 (l. 143).

`tests/edition5/evidence.mjs` is the certificate verifier: gate list at l.
29-38, acceptance ID loop at l. 200-209, visual counts at l. 212-235, perf
counts at l. 237-241.

This is exactly the machinery Edition 8 needs, but its denominator is Edition
5's 32/200/40, not 72/200/40/16.

### 2.3 VS Code harnesses

| Harness | Role |
|---|---|
| `apps/vscode/scripts/selftest.mjs` | Contract/state/handler correctness with a fake DOM; packaged-mode selftest |
| `apps/vscode/scripts/acceptance-edition5.mjs` | Shared A01–A40 executable observations (l. 12) |
| `apps/vscode/scripts/edition5-control-exercise.mjs` | Drives every one of 200 controls; renders screens (l. 12, 382, 421) |
| `apps/vscode/scripts/edition5-dogfood.mjs` | Real daemon, VS Code host runtime flows |
| `apps/vscode/scripts/render-webview.mjs` | Real Chromium render, `--check` measurements, screenshot mode |
| `apps/vscode/scripts/render-edition5.mjs` | Edition-5 screen render |
| `apps/vscode/scripts/verify-vsix.mjs` | Package/extract/IDE-load verification |
| `apps/vscode/scripts/stream-probe.mjs` | 500 messages, 10k tokens, 200 deltas/s; measures messages/renders/nodes/layout reads/queries/mutations/frames; `budget: null` (l. 223) |
| `scripts/vscode-e2e.sh` / `.woodpecker/trusted/trusted.yaml:462` | Real VS Code Extension Host smoke on a pinned Linux VS Code; activates the extension, registers commands, checks webview CSP/composer — **not** a screenshot or workbench pixel comparison |

### 2.4 JetBrains harnesses

| Harness | Role |
|---|---|
| `apps/jetbrains/compile-and-smoke.sh` | Raw-kotlinc fallback + Gradle smokes; known smokes at l. 78; HostMatrix at l. 449 |
| `:frontend:smokeHostMatrixZip` | Packaged ZIP proof; HostMatrix records width/zoom/theme/keyboard-only/states |
| `JetBrainsHostMatrixSmoke.kt` | Widths 240/320/480/800, zoom 100/125/200, themes, keyboard-only, loading/blocked/reconnect; offscreen screenshots at l. 552-609 |
| `JetBrainsParitySmoke` + `visual-baselines.json` | Executable behavioral/visual parity matrix and per-platform digest baseline; the main baseline is linux/macos only |
| `EdtWorkHarness.kt` | 10k-token stream at 200 deltas/s, 500-message transcript, 1k rows, bounded queue/flush assertions (l. 40-45, 61-109) |
| `Edition5ControlExercise.kt` | All 200 controls through real Swing panels |
| `Edition5AcceptanceSmoke.kt` | A01–A40 host observations |

### 2.5 Dogfood

`scripts/edition5/dogfood.sh` is the current model:

1. Build real `faktor-cli` (l. 60-69).
2. `git worktree add --detach HEAD` (l. 71-76) — no dirty checkout trusted.
3. Real compaction/long-task probes.
4. Loopback-only fake OpenAI-compatible provider (l. 92).
5. Real daemon on isolated data dir (l. 107-140).
6. VS Code host runtime against real daemon (l. 142-155).
7. JetBrains smokes against real daemon (l. 157-169).
8. `dogfood.raw.json` with `isolated:true`, `network:"loopback-only"` (l.
   180-183).

It is registered as §7 in `scripts/dogfood.sh:297-301`; the coverage law runs
last (§8, l. 304-307). `scripts/dogfood.mjs` enforces that every repo surface
has a registered gate and that `PLAN_GATES` are executed by a real CI lane
(l. 4-5, 143-184).

> **Reality check from the sibling extraction:** `docs/specs/edition8/screens.md:901-903`
> records that the repository implements the Edition-5 32-screen registry and
> control center; the 40 Rev8-added surfaces S33–S72 have no dedicated screen
> implementation yet. `build-map.csv` marks many controls `partial`. The gaps
> below are therefore not paperwork: most of the 72-screen matrix has to be
> built before its visual/acceptance evidence can exist.

### 2.6 Current release certificate levels

`scripts/certify-local.sh:90-94` distinguishes `local_offline` (clean
full-profile pass on this host) from `release_certified` (local offline plus
the external/cross-platform lanes). `scripts/certify-local.sh:304-316` treats a
dirty checkout as non-certifying. The release path includes package artifacts,
install matrix, signed update manifest, macOS/Windows signing and the
trusted/nightly lanes. None of those levels currently require the Edition-8 UI
counts; that is the release-gate gap, not a missing local runner.

## 3. Gap map (Edition 8)

| # | Gap | Evidence (file:line) | Consequence |
|---|---|---|---|
| 1 | Registry still 32 screens / 200 controls / 40 A | `tests/edition5/registry.mjs:28-32`; `scripts/edition5/run.mjs:75-143` | 40 screens and 16 workflows have no gate |
| 2 | Visual gate still 32 screens / 64 contracts | `tests/edition5/visual-contracts.mjs:2,73,115`; `scripts/edition5/run.mjs:115` | 80 host contracts missing; 288/576 theme vectors absent |
| 3 | Acceptance harness only A01–A40 | `apps/vscode/scripts/edition5-acceptance.mjs:12`; `scripts/edition5/run.mjs:94-106` | W01–W16 have no executable row |
| 4 | VS Code visual evidence is headless Chromium; the real Extension Host smoke explicitly does not display the workbench | `.woodpecker/pr.yaml:304-370`; `.woodpecker/trusted/trusted.yaml:452-471`; `apps/vscode/scripts/render-webview.mjs` | No real VS Code screenshot/token/zoom baseline |
| 5 | JetBrains visual evidence is offscreen Swing, not the IDE | `JetBrainsHostMatrixSmoke.kt:552-609`; `.woodpecker/nightly.yaml:224` | No real IDE window/zoom/theme capture |
| 6 | VS Code macOS/Windows platform records are false | `tests/edition5/visual-contracts.mjs:124-129` | Gate reports `not_certified`; cannot claim 3-OS coverage |
| 7 | The JetBrains baseline read by the Edition-5 gate lacks Windows | `tests/edition5/visual-contracts.mjs:131` reads `visual-baselines.json` (linux/macos); the separate `edition5-visual-baselines.json` has Windows but this gate does not read it; `scripts/check-visual-platforms.mjs:16-19` | Windows is `not_certified`; no inheritance allowed |
| 8 | No assistive-technology run | no NVDA/VoiceOver/Orca harness; `UI_PERF_AUDIT.md:26` is AccessibleContext, not AT | 3.1's screen-reader condition is unmet |
| 9 | No real-host 240/320/480 + 200% matrix for VS Code | `render-webview.mjs --check` is DOM-only; no zoom/width baseline | A39/3.4 cannot pass |
| 10 | Optical audit is reference-only | 3.6 p. 196; no in-host measurement script | "Done (reference renderer)" is not the 30-button host requirement |
| 11 | Contrast verification is token-only | 1.11 p. 179; no rendered-UI contrast harness | 9 host defaults unresolved; HC unverified |
| 12 | Streaming probe has no budget and no CI call | `apps/vscode/scripts/stream-probe.mjs:223` (`budget:null`); `grep` shows no `.woodpecker` use | Performance can regress silently |
| 13 | Perf gate is synthetic Node, not host frame latency | `tests/edition5/perf-delta.mjs:1-20,95-150` | O(n²) is caught, but user-visible p95 is not |
| 14 | No Edition-8 isolated dogfood | `scripts/edition5/dogfood.sh:1-17`; registered only as Edition 5 (§7) | W01–W16 never run in isolation |
| 15 | PR consumes evidence without verifying it | `.woodpecker/pr.yaml:491` (`--no-verify`) vs trusted `.woodpecker/trusted/trusted.yaml:904-906` | PR can merge stale/partial summaries |
| 16 | Certificate counts are hard-coded to Edition 5 | `tests/edition5/evidence.mjs:212-235` | Edition-8 certificate would falsely pass 32/64 |
| 17 | No screenshot artifact for VS Code render | `.woodpecker/pr.yaml` lane marker `artifacts:[]`; `--check` only | No reviewable visual evidence |
| 18 | No high-contrast gate | `visual-contracts.mjs` checks widths/themes as strings only | HC is specified but not exercised |
| 19 | No source-retention/copy evidence gate | 3.6's 1,497/5,973/838 is a design claim | Regressions in moved copy are unmeasured |
| 20 | No 72-screen registry in JetBrains | `Edition5ScreenRegistry.kt`; `visual-contracts.mjs:115-122` asserts 32 | 40 screens absent |

### Top 10 gaps to close first

1. Edition-8 registry (72 screens / 200 controls / A01–A40 / W01–W16).
2. W01–W16 runner in both hosts.
3. Real-IDE visual matrix (VS Code Extension Host + JetBrains IDE), 3 OS.
4. Platform records for VS Code macOS/Windows and JetBrains Windows.
5. AT protocol (NVDA/VoiceOver/Orca) with announcement logs.
6. Real 240/320/480 + 200% + high-contrast host matrix.
7. In-host optical (≥30 buttons/host/theme) and rendered-contrast audit.
8. Performance gate with a recorded baseline and bounded budgets.
9. Edition-8 isolated dogfood (worktree/data dir/loopback daemon).
10. Edition-8 evidence schema/certificate with 72/144/200/40/16/5-platform
    counts, and PR-time verification (not just trusted).

## 4. Performance expectations

### 4.1 What the PDF prints

**There is no numeric UI performance SLO.** The exact qualitative rules are:

> "Maintain keyed windowed transcript rendering, anchored scroll and follow-tail
> state. Page older content and search beyond rendered rows. Coalesce
> high-frequency paint around host frame scheduling while preserving durable
> event order. Cap terminal/log buffers and link artifacts; do not set invented
> performance certifications." (4.2, p. 224)

> "Dispose subscriptions on view/session changes; reject late results by
> binding; page data and coalesce paint. Keep durable event application separate
> from render scheduling. Preserve manual scroll and focus during streaming;
> never rerender the entire form and erase a user's in-progress input." (4.3,
> p. 234)

> A40: "Windowing/coalescing preserve event order/scroll; Find reaches unloaded
> history and logs stay bounded." (4.8, p. 343)

Adjacent numbers are **behaviour fixtures, not performance SLOs**:

- 3 total attempts; 250 ms base; 5,000 ms max; jitter 0.2 (B-06/B-07).
- 60 s quick check / 600 s unit / full background verification (V-08).
- 200% text, 240/320/480/420 widths, 50% expansion (3.1/3.4/4.6).
- 500 messages, 10,000 tokens, 200 deltas/s are the document's own stream
  fixture (A40; `stream-probe.mjs:15-23`), not a latency target.
- 10,000-token/200 deltas/s and 1k rows are the current JetBrains harness
  scenarios (`EdtWorkHarness.kt:40-45`), not a spec SLO.

### 4.2 Current repo measurements (not gates)

- `stream-probe.mjs`: 500-message transcript, one growing assistant entry,
  10,000 tokens as 500 deltas at 200 deltas/s, pinned and scrolled-away;
  records messages/renders/nodes/layout reads/queries/mutations/frames; writes
  `scripts/baselines/stream-probe-*.json`. It sets `budget: null` (l. 223).
- `EdtWorkHarness.kt`: 10k tokens @200/s, 10k burst, 500-message transcript,
  1k rows; asserts one document insert per flush, slow refresh ≤200, flush
  bound, queue bounds, order, 0 drops (`:61-142`).
- `tests/edition5/perf-delta.mjs`: four workloads with relative bounds:
  refresh coalescing (100 vs 10,000 → same small runs), transcript cap/ratio
  ≤4.0×, permission summaries cap + ratio ≤6.0×, Edition-5 patch build linear.
- `apps/jetbrains/UI_PERF_AUDIT.md`: measured before/after (10k tasks →
  1,250 flushes; 8.0×; 10k burst → 40 flushes; 500 messages → 1 insert; 1k rows
  → 0 reset events for identical snapshot; 200 deltas/s → 22 flushes; queue
  bounds). These are evidence, not a CI gate.

### 4.3 Proposed measurable bounds (explicitly not in PDF)

Repo style is baseline-anchored + relative + fail on O(n²)/unbounded, not a
made-up absolute. Proposed gate:

| Workload | Proposed bound | Why this shape |
|---|---|---|
| Stream 500 messages / 10k tokens / 200 deltas/s | p95 delta-to-visible ≤1.25× the committed per-platform baseline; 0 dropped, duplicated or reordered events; 0 full transcript rebuilds per delta; queue/transcript caps hold | Relative to a recorded baseline; preserves A40's order/scroll invariant |
| Input latency during stream | p95 keystroke-to-paint ≤1.25× baseline; no long task >200 ms during 30 s; manual scroll/focus preserved | Existing repo uses ratio/time budgets; this is a proposal |
| Transcript growth | 500 vs 1,000 messages: time ≤2× for 2× input; entries capped at the existing `MAX_TRANSCRIPT_ENTRIES` | Catches O(n²), not noise |
| Search over unloaded history | p95 ≤1.25× baseline; exact match set; no DOM-only false negatives; bounded result page | A40's Find requirement |
| Terminal/log | Buffer cap enforced; oldest artifact linked; RSS plateau over 60 s (≤5% growth after warmup) | "logs stay bounded" |
| Settings/patch build | 10 vs 200 controls: time ratio ≤6× for 20× input (catches the 400× quadratic signature); existing 4×/6× noise bounds | Existing `perf-delta` style |
| Visual | Exact structural/accessibility digest per platform; no pixel tolerance for structural change | Repo's `check-visual-platforms` style |

Every bound must be recorded per host/OS/build/font, with 3 runs and a median;
a missing baseline is `not_certified`, never a pass. An absolute p95 number
(e.g. "16 ms") is **not** proposed here unless the product owner adopts it;
the PDF supplies none.

## 5. Proposed Edition-8 gate and evidence outputs

This is a repo-side design, not a PDF schema.

### 5.1 Gate IDs

```
edition8-registry
edition8-controls-vscode
edition8-controls-jetbrains
edition8-acceptance-vscode
edition8-acceptance-jetbrains
edition8-workflows-vscode
edition8-workflows-jetbrains
edition8-visual-contracts
edition8-visual-platforms
edition8-optical
edition8-contrast
edition8-assistive
edition8-perf
edition8-dogfood
edition8-certificate
```

### 5.2 Evidence layout

```
target/certification/edition8/
  registry.json
  control-exercise-vscode.raw.json
  control-exercise-jetbrains.raw.json
  acceptance-vscode.raw.json
  acceptance-jetbrains.raw.json
  workflows-vscode.raw.json
  workflows-jetbrains.raw.json
  visual-contracts.raw.json
  visual-platforms.raw.json
  optical.raw.json
  contrast.raw.json
  assistive.raw.json
  perf.raw.json
  dogfood.raw.json
  dogfood-vscode.raw.json
  dogfood-jetbrains.raw.json
target/certification/edition8-summary.json
target/certification/lanes/edition8.json
target/certification/ci-certification.json   (existing aggregate)
```

Recommended schemas (repo convention, not PDF):

- `faktor-edition8-gate/v1`
- `faktor-edition8-certificate/v1`
- `faktor-edition8-visual/v1`
- `faktor-edition8-assistive/v1`
- `faktor-edition8-perf/v1`
- `faktor-edition8-dogfood/v1`

Every document carries `commit`, `tree`, `dirty`, `generated_at`, host/OS/
build/font, exact commands digest, artifact sha256, counts, results, and
explicit `not_certified` reasons. No platform may inherit another platform's
record. No host may inherit another host's pass.

### 5.3 Release criteria

An Edition-8 release claim requires, at minimum:

1. 72/72 screens and 144/144 host contracts present.
2. 200/200 controls exercised in both hosts, 0 missing/orphan/dead.
3. 40/40 A scenarios pass in both hosts.
4. 16/16 W workflows pass all five cells in both hosts.
5. 5 platform records (VS Code win/mac/linux; JetBrains win/mac/linux) or an
   explicit `not_certified` that blocks the claim.
6. Four host themes + high contrast, 240/320/420/480 + 200%.
7. AT records for NVDA/VoiceOver/Orca.
8. Optical ≥30 buttons/host/theme; contrast resolved or reported.
9. Performance baselines present and within the adopted bound.
10. Isolated dogfood with real daemon, loopback-only provider, clean
    worktree/data dir.
11. Zero forbidden fake states from `acceptance.md` §7.
12. PR-time verification of the evidence, not only trusted/nightly.

## 6. Recommended verification order

1. **Freeze the registry** (72 screens, 200 controls, A01–A40, W01–W16,
   host surfaces, control→screen maps). Fail if counts drift.
2. **Compile both hosts** and run the existing 200-control exercise against
   the Edition-8 registry (control existence/handlers first).
3. **Run A01–A40** in both hosts with shared observations; fail on any
   missing/duplicate ID or forbidden fake state.
4. **Run W01–W16** pointer + keyboard + screen reader + 200%/320 + recovery;
   record five cells per host.
5. **Run the visual matrix** (72 × 2 hosts × four themes, HC separately) on
   each platform; exact digests; real-host screenshots for native surfaces.
6. **Optical + contrast** in the real hosts (≥30 buttons/host/theme).
7. **AT protocol** (NVDA/VoiceOver/Orca), announcement logs, focus
   restoration.
8. **Performance** (baseline first, then gate) and **dogfood** (isolated
   worktree/data dir/daemon).
9. **Emit the certificate**, verify counts/currentness/platform binding, and
   run the PR-time verifier.

Rationale: registry and controls are the cheapest failures; acceptance before
visual, because a wrong state makes a screenshot meaningless; visual before
performance, because layout regressions often cause the perf symptoms; dogfood
last, because it depends on all host adapters.
