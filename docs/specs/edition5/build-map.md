# Edition 5 build map and construction plan

Source: `Faktor_IDE_UI_UX_2.pdf`, "Native implementation and delivery" p.024, "Required construction plan" pp.025–037, code outlines pp.038–050, and the control catalog pp.051–100.

## Provenance and missing CSV

The PDF requires a handoff containing `Build_Map.csv`, `implementation/*.rs`, `*.ts`, `*.kt` outlines and spacing tokens (p.024, repeated at the foot of pp.025–037). **Those files are not present in this repository.** This document plus the reconstructed `build-map.csv` (200 rows + header, derived from `controls.json`) reconstructs the machine-readable build map from:

- the 13 build packages and ownership rules (pp.025–026);
- the 200 control rows' `BUILD` tags (`controls.json` → `build_packages`);
- the reference contracts and code outlines (pp.027–050);
- the spacing/typography rules (p.035) and host CSS outline (p.050).

Do not treat the reference code as integrated production code. The PDF explicitly says the outlines are "deliberately unintegrated reference code; actual repository types, async primitives and error enums must be reused or extended rather than duplicated" (p.025). No mock may become the production source of model usage, permissions, saved settings or completion (p.025).

## 1. Build packages and ownership (pp.025–026)

| Package | Owns | Must not own | Governing pages |
|---|---|---|---|
| **BK01** Configuration | Descriptors, effective snapshots, atomic patches, secret references, apply state | Provider transport, UI precedence | pp.026–027, 038–039 |
| **BK02** Models/providers | Provider lifecycle, capability/parameter mapping, normalized call receipts | Secret plaintext, UI policy | pp.028, 039 |
| **BK03** Customization | Trusted versioned presets/rules/memory/skills, deterministic instruction manifests | Tool execution, permission grants | pp.029, 043 |
| **BK04** Context | Exact context/accounting, source identity, compaction | UI estimate as measurement | pp.029, 040 |
| **BK05** Authority | Effective policy, simulation, enforcement, truthful sandbox status | Persistent approval waiter only (BK08) | pp.030, 041 |
| **BK06** Tools/MCP/hooks | Tool/MCP/hook lifecycle, auth, bounded process effects | Permission broadening from install/enable | pp.030, 042 |
| **BK07** Budgets/reliability | Shared money/token reservations, routing/retries/efficiency reads and changes | Task admission identity | pp.032, 044 |
| **BK08** Sessions/recovery | Session/queue/process/remote identity, generation fencing | Product policy/ledger decisions | pp.031, 045 |
| **BK09** Review/proof | Review/restore/fork/evidence/finish | Host diff rendering | pp.032, 046 |
| **BK10** Diagnostics/optional services | Runtime trust, readiness, diagnostics, updater/enterprise/cloud/worker services | Local preference overrides of managed policy | pp.031, 047 |
| **UI01** VS Code adapter | Host presentation/controller over the same semantic fixtures | Policy/ledger decisions in webview | pp.048, 050 |
| **UI02** JetBrains adapter | Host presentation/controller over the same semantic fixtures | Policy/ledger decisions in Swing events | pp.049–050 |
| **UI03** Visual system | Spacing/typography/overflow tokens and visual acceptance gate | Feature semantics | p.035, p.050 |

The PDF says UI01/UI02 are "host adapters over the same semantic fixtures"; the language of the outlines makes UI01 the TypeScript/VS Code adapter and UI02 the Kotlin/JetBrains adapter. This host mapping is inferred from the outline languages and host sections, not stated in a single table—recorded as an ambiguity.

## 2. Seam contracts (pp.027–032, 038–047)

| Reference contract | Core decision | Transaction / acknowledgement | Current repository seam |
|---|---|---|---|
| `contracts/configuration.rs` | Typed patch decision from current revision + descriptors + policy + capability snapshot; full error list or immutable validated patch; unknown keys fail | Compare expected revision inside the store transaction; persist all or none; saved-but-not-applied is first-class | `crates/cli/src/config/*`; no native patch/effective snapshot route (`docs/native-protocol.md:110`) |
| `contracts/model_catalog.rs` | `ModelDescriptor`, parameter descriptors, `capabilityAuthority` (`known` / `declared_generic` / `unknown`) | Capability snapshot version + normalized call receipt | `GET /models`, `GET /capabilities`, `GET /native/providers` |
| `contracts/context.rs` | `Source` identity (URI/range/version/hash), versioned manifest, accounting categories with one basis | Context revision; compaction is its own durable operation | `context/src/budget.rs`; projection `contextUsage` is null (`docs/native-protocol.md:138`) |
| `contracts/authority.rs` | One effective policy decision for preview and dispatch; managed denies; sandbox status | Persistent policy store revision; one-shot waiter generation | `POST /native/permission/reply`, `crates/server/src/permission.rs:58`; no persistent store |
| `contracts/tools.rs` | Tool identity = server instance + stable tool name; effect class; bounded stdio spec | Explicit start/restart acknowledgement; remote transport capability negotiation | `crates/cli/src/config/mcp.rs:7`; tool calls in runs |
| `contracts/customization.rs` | `VersionedContent` for presets/rules/memory/skills; source/version/scope | Install/update/delete receipt; prior known-good version | `crates/instructions/src/lib.rs:1479`; `crates/memory/src/lib.rs:31`; hooks |
| `contracts/budget.rs` | Integer micro-USD parse; shared parent/child reservation; eligible attempted spend | Admission receipt UUID + body hash; reservation/settlement/refund | `crates/orchestrator/src/lib.rs:2177`; VS Code budget settings |
| `contracts/operations.rs` | Binding tuple (`session`, `worktree`, `daemon`, generation); owned queue/process identity | Durable admission/finish receipt; `effect_status = unknown` on uncertain effects | `apps/vscode/src/lifecycleFence.ts:9`; terminal routes |
| `contracts/review.rs` | Candidate/proof identity; content-aware restore; `expected_current_hash` | CAS transaction; fork/restore separate | `crates/session/src/checkpoints.rs:18`; `changeReview.ts:33` |
| `contracts/diagnostics.rs` | Allowlisted runtime status; redacted export | Explicit start/export acknowledgement; checksum ≠ signature verification | `GET /native/health/ready`, updater/diagnostics; no export route |
| `ui/state-contracts.ts` | Staged form model, descriptors/snapshots read once, one validated patch | Per-field apply state (`Effective`/`NextCall`/`NextTask`/`NewSession`/`RestartRequired`/`Failed`) | `apps/vscode/src/webview.ts`; `cockpit.ts` |
| `ui/layout.css` | `--s1..--s8`, control/label/help/error slots, overflow rules | Visual acceptance gate | `apps/vscode/media/` + JetBrains `PanelSupport`/JBUI |
| `ui/controller.ts` / `controller.kt` | Host adapter; selection/focus/draft restoration; generation fence | Host action receipts; no policy/ledger logic | `nativeClient.ts`, `FaktorChatPanel.kt`/`FaktorFrontendService.kt` |

## 3. Build tags by category (from `controls.json`)

Every control row carries one or more package IDs. This is the package load, not a release order.

| Category | Packages named by control rows (union) |
|---|---|
| M Models/routing | BK01, BK02, BK07, BK10, UI01–UI03 |
| P Providers | BK01, BK02, BK10, UI01–UI03 |
| G Generation | BK01, BK02, UI01–UI03 |
| A Agents/presets | BK01, BK03, BK05, BK08, UI01–UI03 |
| C Context | BK01, BK03, BK04, BK07, UI01–UI03 |
| K Compaction/memory | BK01, BK03, BK04, BK07, UI01–UI03 |
| I Instructions/rules | BK01, BK03, BK05, UI01–UI03 |
| Q Permissions/sandbox | BK01, BK05, BK06, BK08, BK10, UI01–UI03 |
| T Tools/MCP | BK01, BK05, BK06, BK08, BK10, UI01–UI03 |
| S Skills/hooks | BK01, BK03, BK05, BK06, BK08, UI01–UI03 |
| X Terminal/browser/LSP | BK05, BK06, BK08, BK10, UI01–UI03 |
| B Budgets/reliability | BK01, BK02, BK07, BK08, BK10, UI01–UI03 |
| V Changes/checkpoints/verification/finish | BK01, BK03, BK07, BK09, UI01–UI03 |
| D Sessions/display/accessibility | BK01, BK03, BK08, BK09, UI01–UI03 |
| O Privacy/diagnostics/services | BK01, BK02, BK03, BK07, BK08, BK10, UI01–UI03 |

The authoritative per-control list is in `controls.json` (`build_packages`); `build-map.csv` is a flat projection of the same records for tools that want one CSV row per control. Do not infer package ownership from the category summary alone.

## 4. Spacing, typography and overflow tokens (p.035, p.050)

```css
--s1: 4px;   --s2: 8px;   --s3: 12px;  --s4: 16px;
--s6: 24px;  --s8: 32px;
--control-min: 32px;      /* compact host convention may use 28 */
--field-gap: 12px;
--panel-inset: 12px;      /* 16 standard, 24 wide */
--body-leading: 1.45;     /* 1.4–1.5 */
--label-leading: 1.3;     /* 1.25–1.35 */
```

- Label/value form row: `grid-template-columns: minmax(10rem, 15rem) minmax(0, 1fr)` only when space allows; stack below the measured breakpoint.
- Use `min-width: 0` and `overflow-wrap: anywhere` for IDs/paths; never clip labels to a fixed single-line box.
- Control spacing: inline 4/8, controls 8, helpers 4, form rows 12, groups 24; no off-grid helper text.
- Breakpoints: 240–319 vertical labelled stack with Send/Stop/attention visible; 320–479 two compact wrapped rows; ≥480 full configuration summary (p.023).
- No brand fonts in IDE reading controls; inherit host font family/size/zoom; semantic colors/focus/high-contrast; no blinking/decorative motion; reduced motion removes nonessential transitions.
- Host mapping: VS Code CSS variables above; JetBrains uses JBUI/UIManager metrics and host-scaled icons (p.050). UI03 is a release requirement (p.035).

## 5. Fixtures and harnesses (pp.023–025, 036–037)

- **Semantic fixtures:** capability authority known/declared/unknown; model parameter ranges; provider health/auth/discovery; context and compaction; permissions/sandbox; MCP stdio/remote; budget/retry; restore/fork; host binding. Both hosts must consume the same fixtures (p.024).
- **Deterministic state fixtures:** populated, empty, large/long, invalid, stale, unavailable, disconnected, queued, failed, effect-unknown. Visual review uses these before merge (p.036).
- **Host matrix:** 240/320/480/800 px, 200% text, keyboard-only, IME, both themes/high contrast, reduced motion, screen reader names (pp.023, 036, 039).
- **Current repository harnesses:**
  - `apps/vscode/scripts/selftest.mjs` — native DTO/stream/state/fence/webview tests;
  - `apps/vscode/scripts/render-webview.mjs` — deterministic 240/320/480/800 visual baselines and focus/pointer checks;
  - `apps/jetbrains/compile-and-smoke.sh` — Backend, NativeBridge, Frontend, ControlPlaneCredential, JetBrainsParity, HostMatrix, EdtWork;
  - `apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json` — pinned linux/macos/windows digests;
  - `apps/jetbrains/UI_PERF_AUDIT.md` — EDT/stream/transcript/list performance evidence;
  - `tests/static-authority` — Faktor-owned source/branding boundary.
- **Fixtures to add:** W01–W16 end-to-end scenario fixtures with fake daemon effects (no production effects), A01–A40 assertion fixtures, and a dogfood driver that exercises both hosts against one daemon.

## 6. Required construction plan and gates (pp.024–037)

### Gate 0 — Contract and fixture freeze
- Freeze the binding tuple (`session`, `worktree`, `daemon`, generation), capability/authority vocabulary, context accounting basis, permission decision and operation identity.
- Freeze the deterministic fixtures and screenshot baselines; no feature effect may be enabled before server capability negotiation.
- **Exit:** every control has an owner/package, every screen references its control IDs, and both host adapters can render the fixtures without claiming runtime effects.

### Gate 1 — Configuration/models (BK01–BK03)
- Typed configuration descriptors, effective snapshot, atomic patch, secret references and per-field apply state.
- Provider lifecycle, capability/parameter mapping, call receipts, trusted versioned presets/rules/memory/skills.
- **Exit:** S02–S07, S11, S17, S29 can stage/validate/apply through the daemon with no local precedence logic.

### Gate 2 — Context/authority/tools (BK04–BK06)
- Context manifest/accounting and owned compaction; one policy evaluator for preview/dispatch plus persistent rule store; MCP/hook lifecycle with bounds, trust and truthful health.
- **Exit:** S08–S10, S13–S18, S22 can explain exactly what enters a call, what is allowed and what is running.

### Gate 3 — Execution/proof (BK07–BK10)
- Shared reservations/retries/efficiency; session/queue/process/remote identity; review/restore/fork/evidence/finish; diagnostics/optional services.
- **Exit:** S19–S28, S30–S32 can complete W12–W16 with durable receipts and no fake green states.

### Gate 4 — Screens/flows and dogfood
- Implement every S01–S32 control set in both hosts; exercise W01–W16 on one daemon; run A01–A40 and the visual/host matrix; add the dogfood driver.
- **Exit:** both hosts pass the same behavioral fixtures; visual baselines re-pinned deliberately; all 40 scenarios pass or are explicitly blocked with evidence.

### Required per-row verification (p.176)
For every control row: default, field validation, transactional apply, apply timing, ownership fence, unavailable/loading/error/stale state, recovery, keyboard/help/source and the matching acceptance scenario. A row is not "done" because a widget renders.

## 7. Page index

| Content | Pages |
|---|---|
| Native delivery obligation, Build_Map requirement | 024 |
| Backend packages/ownership | 025–026 |
| Configuration contract | 026–027 |
| Provider/contracts | 028 |
| Context/compaction | 029 |
| Authority/tools/customization | 030 |
| Operations/diagnostics | 031 |
| Budgets/review | 032 |
| Surface/tokens/seams | 033–036 |
| Definition of ready/reuse/tests | 037 |
| Rust code outline | 038–047 |
| TypeScript/Kotlin/CSS outlines | 048–050 |
| 200 control rows | 051–100 |
| Screen contracts | 101–108 |
| Native visual plates | 109–172 |
| Acceptance/evidence | 173–183 |

## Implemented harness map (Edition 5 gates)

- **Control exercise (both hosts):** `apps/vscode/scripts/edition5-control-exercise.mjs`
  drives the shipped `src/generated/edition5Registry.ts` + `src/edition5/*`
  bindings for all 200 controls; `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition5ControlExercise.kt`
  walks the real Swing panel trees. Both fail on an unexercised control, a
  missing binding/handler, or an rendered control without an observable effect.
- **Acceptance A01–A40:** `apps/vscode/scripts/edition5-acceptance.mjs` and
  `apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/Edition5AcceptanceSmoke.kt`
  feed the shared field-level assertions in `tests/edition5/acceptance.mjs`.
- **Dogfood:** `scripts/edition5/dogfood.sh` uses an isolated `git worktree` +
  data dir, a loopback-only fake provider and the real daemon; it runs the
  real compaction/runtime long-task probes and the VS Code host runtime, and
  the JetBrains host smokes run against the real daemon in the pipeline.
- **Contract gates:** `tests/edition5/{visual-contracts,settings-contract,perf-delta,dead-control}.mjs`
  write machine-readable evidence consumed by
  `scripts/edition5/run.mjs` and `tests/edition5/evidence.mjs`.
