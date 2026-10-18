# Edition 5 implementation backlog

Source: `Faktor_IDE_UI_UX_2.pdf` (Edition 5, 17 Oct 2026). Control status is derived from the repository scan recorded in `controls.json`.

## Current-state snapshot

- **200 controls:** 25 `exists`, 105 `partial`, 70 `missing`.
- **32 screens:** no screen has its full Edition 5 control set in both hosts. Existing VS Code and JetBrains surfaces cover parts of Work/Inspect/History, provider/model selection, task options, changes, verification, agents, terminal, approvals, usage/outcomes and settings.
- **16 flows:** W01–W16 have partial support only where they overlap the existing Work/Inspect/History path; no flow is fully dogfooded.
- **40 acceptance scenarios:** none are executed against a product build in the PDF (p.173); the current harnesses cover existing surfaces, not the new screens/flows.

Per-category status (from `controls.json`):

| Category | exists | partial | missing | Main blocker |
|---|---:|---:|---:|---|
| A Agents/presets | 1 | 11 | 0 | Presets/roles are labels over existing task/agent controls |
| B Budgets/reliability | 2 | 9 | 1 | No budget revision/queue/uncertain-admission UI |
| C Context | 1 | 8 | 5 | `contextUsage` is null; no manifest/accounting |
| D Sessions/display | 6 | 5 | 4 | No transcript Find, rename/archive, parallel tabs |
| G Generation | 0 | 3 | 11 | No capability-driven parameter API/editor |
| I Instructions/rules | 0 | 3 | 8 | No effective-order/import/enable UI |
| K Compaction/memory | 0 | 7 | 5 | No compaction operation, memory CRUD or effect display |
| M Models/routing | 0 | 12 | 2 | Config-backed only; no capability/routing/identity sheet |
| O Privacy/diagnostics | 4 | 5 | 4 | No settings search/import/export/retention/telemetry |
| P Providers | 0 | 12 | 3 | No connection CRUD/probe/vault/rotation |
| Q Permissions/sandbox | 1 | 9 | 4 | One-shot replies only; no persistent policy/simulator |
| S Skills/hooks | 0 | 7 | 7 | Hooks exist backend-only; no library/install/test UI |
| T Tools/MCP | 0 | 7 | 8 | Stdio config only; no CRUD/remote/OAuth/health |
| V Changes/checkpoints | 4 | 5 | 3 | No restore/fork/partial acceptance |
| X Terminal/browser/LSP | 6 | 2 | 5 | Terminal/LSP exist; browser adapter absent |

## Top 10 gaps (ordered by dependency)

1. **No versioned configuration authority.** `docs/native-protocol.md:110` has no effective-snapshot or atomic-patch route; both hosts have small host-local settings (`apps/vscode/package.json:117`, `apps/jetbrains/.../FaktorSettingsModel.kt:45`). Blocks M-01..M-14, G-01..G-14, P-01..P-15, S-29, A-37. Spec pp.010, 026–027, 038.
2. **No durable context accounting.** `docs/native-protocol.md:138` says `contextUsage` is null. Blocks C-01/C-02, K-01..K-04, S-08/S-10, A-11/A-12/A-15. Spec pp.010, 029, 040.
3. **No provider lifecycle/vault.** `crates/cli/src/config/provider.rs:31` is config-only; no test/rotate/delete/secret handoff. Blocks P-01..P-15, W01, A-01/A-06/A-07. Spec pp.028, 039.
4. **No capability/parameter descriptors or call receipts.** `GET /models` is a flat catalog; no adapter ranges/effective request receipt. Blocks G-01..G-14, M-07..M-14, A-02..A-05. Spec pp.028, 039.
5. **No persistent permission policy.** `crates/server/src/permission.rs:58` owns one-shot waiters; `POST /native/permission/reply` cannot save rules. Blocks Q-02/Q-07/Q-08/Q-09, A-17/A-18. Spec pp.030, 041.
6. **No MCP/hook/skill lifecycle management.** `crates/cli/src/config/mcp.rs:7` is stdio-only; `crates/hooks/src/lib.rs:34` has no management route; `crates/instructions/src/lib.rs:1479` discovers skills only. Blocks T-03..T-15, S-01..S-14. Spec pp.030, 042–043.
7. **No memory CRUD/retention.** `crates/memory/src/lib.rs:31` stores facts; no native list/edit/delete or retention UI. Blocks K-10/K-11, O-09, A-16. Spec pp.029, 043.
8. **No restore/fork/partial acceptance.** `crates/session/src/checkpoints.rs:18` records hashes; no restore preview/fork/redo. Blocks V-03/V-05/V-06, A-33/A-34. Spec pp.032, 046.
9. **No browser or full transcript/search surface.** No browser adapter route; History is title-filtered (`apps/jetbrains/.../HistoryPanel.kt:73`); no Find across unloaded pages. Blocks X-06..X-09, D-03/D-15, A-40. Spec pp.031, 045.
10. **No unified settings/search/import/diagnostics export.** Settings are category-local; no effective-source search, import/export, retention/diagnostics bundle. Blocks O-01/O-03/O-09/O-12, A-37. Spec pp.027, 047.

## Wave 0 — Freeze artifacts and fixtures (no UI effects)

**Depends on:** none. **Exit:** the artifacts in `docs/specs/edition5/` are reviewed; deterministic fixtures and screenshot baselines are agreed; no runtime behavior changes.

- **0.1** Freeze IDs and counts: 200 controls, 32 screens, 16 flows, 40 A scenarios; publish `controls.json`, `screens.md`, `flows.md`, `acceptance.md`, `build-map.md`. Responsible: `docs/specs/edition5/`. Spec pp.051–176.
- **0.2** Freeze binding tuple and capability vocabulary: session/worktree/daemon/generation; `known`/`declared_generic`/`unknown`; `Effective`/`NextCall`/`NextTask`/`NewSession`/`RestartRequired`/`Failed`. Responsible: `docs/native-protocol.md`, `apps/vscode/src/lifecycleFence.ts:9`, `apps/jetbrains/shared/.../NativeProtocol.kt:2`. Spec pp.010–012, 031.
- **0.3** Build deterministic semantic fixtures for both hosts: empty/large/invalid/stale, provider auth/discovery, context/compaction, permission/sandbox, MCP/hook, budget/retry, restore/fork. Responsible: `apps/vscode/scripts/render-webview.mjs:52`, `apps/jetbrains/frontend/src/test/.../JetBrainsParityMatrix.kt:968`. Spec pp.024, 036.
- **0.4** Decide and record the Build_Map.csv reconstruction policy (no CSV in repo; `controls.json` is authoritative per-control). Responsible: this directory, `build-map.md`. Spec p.024.

## Wave 1 — Foundation: contracts and authority (must precede controls)

**Depends on:** Wave 0. **Exit:** pure decision types, versioned snapshots, receipts and generation fences exist behind feature flags; no screen claims effects yet.

- **1.1 Configuration service (BK01).** Typed descriptors, effective snapshot, atomic patch, expected revision, per-field apply state, write-only secret binding. Responsible: `crates/cli/src/config/*`, new native routes in `crates/server/src`, `docs/native-protocol.md`, `apps/vscode/src/nativeClient.ts:3939`, `apps/jetbrains/.../FaktorFrontendService.kt`. Spec pp.026–027, 038; controls M-01..M-14, G-01..G-14, P-01..P-15, S-29.
- **1.2 Model/provider authority (BK02).** Capability descriptor, parameter ranges, authority provenance, provider lifecycle, normalized call receipt. Responsible: `crates/router/src/lib.rs:108`, `crates/provider/src/config.rs:1`, `GET /models` / `GET /capabilities` / `GET /native/providers`, `apps/vscode/src/nativeClient.ts:3987`. Spec pp.028, 039.
- **1.3 Context manifest/accounting (BK04).** Source identity, one-basis categories, reserves, versioned manifest; replace nullable projection with a versioned read or keep `Unavailable` until it exists. Responsible: `crates/context/src/budget.rs:9`, `docs/native-protocol.md:138`, `GET /session/{id}/projection`. Spec pp.029, 040.
- **1.4 Authority evaluator/store (BK05).** One decision for preview/dispatch; managed denies; persistent versioned rules; simulator; safe revocation. Responsible: `crates/server/src/permission.rs:58`, `crates/cli/src/config/sandbox.rs:49`, `POST /native/permission/reply`. Spec pp.030, 041.
- **1.5 Operations/identity fencing (BK08).** Binding tuple, queue/process/remote identity, generation fencing, unknown-effect state. Responsible: `apps/vscode/src/lifecycleFence.ts:9`, `crates/orchestrator/src/lib.rs:585`, terminal routes. Spec pp.031, 045.
- **1.6 Budget ledger/idempotent admission (BK07).** Exact micro-USD, shared reservations, settlement/refund, admission UUID/body hash. Responsible: `crates/orchestrator/src/lib.rs:2177`, `crates/cli/src/config/core.rs:99`, task-run routes. Spec pp.032, 044.
- **1.7 Proof/review identity (BK09).** Candidate/proof identity, content-aware restore/fork, finish receipts. Responsible: `crates/session/src/checkpoints.rs:18`, `apps/vscode/src/changeReview.ts:33`. Spec pp.032, 046.
- **1.8 Diagnostics/optional services (BK10).** Runtime status, allowlisted export, optional service truth. Responsible: `docs/native-protocol.md:124`, `crates/cli/src/config/enterprise.rs:27`, `crates/cli/src/config/workers.rs:35`. Spec pp.031, 047.

## Wave 2 — Control families (backend → host surface)

**Depends on:** Wave 1. Implement in this order; each item includes its controls, screens, acceptance and current area. Do not start a host form until its backend contract exists; use fixtures meanwhile.

- **2.1 Settings shell and atomic apply (S29).** O-01, O-04, O-07, O-08, O-10, O-12, O-13, D-09, D-10, D-11, D-14. Responsible: new VS Code settings webview/editor (`apps/vscode/src/webview.ts:294`), JetBrains `FaktorConfigurable.kt:58` / `FaktorSettingsModel.kt:170`, `crates/cli/src/config/*`. Spec pp.027, 101, 165–166; A37.
- **2.2 Provider/catalog controls (S02–S04).** M-01..M-14, P-01..P-15, C-10, M-09. Responsible: `apps/vscode/package.json:117`, `apps/jetbrains/.../SettingsPanel.kt:25`, `crates/cli/src/config/provider.rs:31`, `apps/vscode/src/nativeClient.ts:3987`. Spec pp.028, 051–058, 111–116; A01, A02, A06, A07, A08.
- **2.3 Generation tuning (S05).** G-01..G-14. Responsible: `crates/agent` request DTOs, `POST /native/session/{id}/task-runs`, VS Code webview composer, JetBrains task options. Spec pp.028, 059–061, 117–118; A03, A04, A05.
- **2.4 Context and compaction (S08–S10).** C-01..C-14, K-01..K-04, I-11, V-07. Responsible: `crates/context`, `crates/session`, new native manifest/compaction operations, `apps/vscode/src/webview.ts:471` Inspect, JetBrains Inspect. Spec pp.029, 071–078, 123–128; A11–A15.
- **2.5 Permissions/sandbox/approval (S13–S14).** Q-01..Q-14, X-12, B-12, D-12. Responsible: `crates/server/src/permission.rs:58`, `crates/cli/src/config/sandbox.rs:49`, approval dock in `apps/vscode/src/webview.ts:356`, JetBrains approval cards. Spec pp.030, 085–090, 133–136; A17–A20.
- **2.6 Budgets/retry/loop (S21).** B-01..B-12. Responsible: `apps/vscode/package.json:140`, task admission DTOs, `crates/cli/src/config/core.rs:99`, usage routes. Spec pp.032, 099–100, 149–150; A30–A32.
- **2.7 MCP/tools (S15–S16).** T-01..T-15. Responsible: `crates/cli/src/config/mcp.rs:7`, new native CRUD/health routes, `apps/vscode` settings/Inspect, JetBrains settings. Spec pp.030, 091–096, 137–140; A21–A23.
- **2.8 Skills/commands/hooks (S17–S18).** S-01..S-14, I-06. Responsible: `crates/instructions/src/lib.rs:1479`, `crates/hooks/src/lib.rs:34`, new management/test routes. Spec pp.030, 097–098, 141–144; A23, A24.
- **2.9 Instructions/rules/memory (S11–S12).** I-01..I-11, K-05..K-11, O-09. Responsible: `crates/instructions`, `crates/memory/src/lib.rs:31`, `crates/cli/src/config/efficiency.rs:74`, new native CRUD. Spec pp.029–030, 079–084, 129–132; A10, A15, A16.
- **2.10 Agents/presets/delegation (S06–S07, S23).** A-01..A-12, M-05, M-11, Q-06, B-04. Responsible: `crates/orchestrator/src/lib.rs:585`, `apps/vscode/src/nativeClient.ts:4138`, JetBrains `AgentsPanel.kt:22`, task options. Spec pp.030, 065–070, 119–122, 153–154; A09, A10, A25, A26.

## Wave 3 — Screen completion (both hosts)

**Depends on:** the relevant Wave 2 family. Each screen is "done" only when its exact control list renders and its states/fixtures pass. Every screen is both-host (p.101–108).

- **3.1 S01 Work cockpit** — A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10. Files: `apps/vscode/src/webview.ts:356`, `apps/jetbrains/.../FaktorChatPanel.kt:111`; spec pp.101, 109–110.
- **3.2 S02 Model picker** — M-01..M-14. Spec pp.101, 111–112.
- **3.3 S03 Provider connection** — P-01..P-06, P-10..P-12, P-14. Spec pp.101, 113–114.
- **3.4 S04 Local setup** — P-07..P-09, P-13, P-15, C-10, M-09. Spec pp.101, 115–116.
- **3.5 S05 Model tuning** — G-01..G-14. Spec pp.102, 117–118.
- **3.6 S06 Agent presets** — A-01..A-12. Spec pp.102, 119–120.
- **3.7 S07 Custom agent** — A-04..A-09, A-11, A-12, M-05. Spec pp.102, 121–122.
- **3.8 S08 Context inspector** — C-01, C-02, C-04, C-05, C-07..C-14. Spec pp.102, 123–124.
- **3.9 S09 Mentions/attachments** — C-03..C-08, C-13, X-10, X-11. Spec pp.103, 125–126.
- **3.10 S10 Compaction review** — K-01..K-04, C-12, I-11, V-07. Spec pp.103, 127–128.
- **3.11 S11 Rules/instructions** — I-01..I-11. Spec pp.103, 129–130.
- **3.12 S12 Memory** — K-05, K-10, K-11, O-09, I-11. Spec pp.103, 131–132.
- **3.13 S13 Permissions/sandbox** — Q-01..Q-05, Q-07..Q-14. Spec pp.104, 133–134.
- **3.14 S14 Approval** — Q-06, Q-07, Q-09, Q-13, X-12, B-12, D-12. Spec pp.104, 135–136.
- **3.15 S15 Tools/MCP health** — T-01..T-04, T-06..T-13, T-15. Spec pp.104, 137–138.
- **3.16 S16 Add MCP** — T-03..T-06, T-10, T-13..T-15. Spec pp.104, 139–140.
- **3.17 S17 Skills/commands** — S-01..S-06, S-14, I-06. Spec pp.105, 141–142.
- **3.18 S18 Hooks** — S-07..S-14. Spec pp.105, 143–144.
- **3.19 S19 Terminal ownership** — X-01..X-05, X-12, D-07, B-10. Spec pp.105, 145–146.
- **3.20 S20 Browser/diagnostics/editor actions** — X-06..X-11, X-13, C-03. Spec pp.105, 147–148.
- **3.21 S21 Budgets/retry/loop** — B-01..B-12. Spec pp.106, 149–150.
- **3.22 S22 Live run** — D-05, D-07, D-08, D-12, D-13, M-11, G-14, Q-06, T-09, B-04. Spec pp.106, 151–152.
- **3.23 S23 Agents** — A-06..A-12, M-11, B-04, Q-06. Spec pp.106, 153–154.
- **3.24 S24 Changes/review** — V-01..V-03, V-07, V-09, V-11, V-12, A-11. Spec pp.106, 155–156.
- **3.25 S25 Checkpoints/restore/fork** — V-04..V-06, V-12, D-06, Q-09. Spec pp.107, 157–158.
- **3.26 S26 History/search/parallel** — D-01..D-04, D-06, D-13, D-15, V-09. Spec pp.107, 159–160.
- **3.27 S27 Usage/outcomes** — B-04, M-13, K-12, O-06, V-09, D-07. Spec pp.107, 161–162.
- **3.28 S28 Routing/efficiency/retrieval** — M-04, M-06, M-10, K-05..K-09, K-12, C-09..C-11. Spec pp.107, 163–164.
- **3.29 S29 Settings** — O-01, O-04, O-07, O-08, O-10, O-12, O-13, D-09, D-10, D-11, D-14. Spec pp.108, 165–166.
- **3.30 S30 Diagnostics** — O-03, O-04, O-05, O-07, O-09, O-13, B-11, B-12, D-13. Spec pp.108, 167–168.
- **3.31 S31 Remote workspaces** — X-13, P-13, O-02, O-07, O-11, D-01, D-15, A-11. Spec pp.108, 169–170.
- **3.32 S32 Task outcome/finish** — V-07..V-12, O-06, B-04. Spec pp.108, 171–172.

## Wave 4 — Flow completion (W01–W16)

**Depends on:** the screens each flow traverses. Add one end-to-end fixture per flow; no mock result may stand in for a runtime acknowledgement.

- **4.1 W01** connect/validate custom provider — S03→S02→S05; files `SettingsPanel.kt`, new settings webview, config service. Spec p.014; controls P-*, M-*, G-*.
- **4.2 W02** local model without false locality — S04→S31→S08; `crates/ollama/src/lib.rs:303`, binding/context routes. Spec p.014.
- **4.3 W03** tune/change mid-session — S02→S05→S01; model receipt + task options. Spec p.015.
- **4.4 W04** Ask→Plan→Code — S06→S07→S01; task admission/preset enforcement. Spec p.015.
- **4.5 W05** context assembly/overflow — S09→S08→S10; context manifest/attachments. Spec p.016.
- **4.6 W06** compaction continuity — S10→S08→S32; new compaction operation. Spec p.016.
- **4.7 W07** approve once/rule — S14→S13→S22; policy store/waiter. Spec p.017.
- **4.8 W08** MCP trust/auth — S16→S15→S13; MCP CRUD/health. Spec p.017.
- **4.9 W09** skill install/invoke — S17→S11→S13; skill manifest/trust. Spec p.018.
- **4.10 W10** hook author/test — S18→S30→S22; hook management/test. Spec p.018.
- **4.11 W11** child delegation/steer — S07→S23→S14; orchestrator controls. Spec p.019.
- **4.12 W12** queue/background/interrupt — S22→S19→S26; queue/process scopes. Spec p.019.
- **4.13 W13** rate limit/cap/unknown spend — S21→S22→S27; ledger/retry. Spec p.020.
- **4.14 W14** review/restore/fork — S24→S25→S26; restore/fork/CAS. Spec p.020.
- **4.15 W15** finish with proof — S01→S24→S32; proof/finish receipts. Spec p.021.
- **4.16 W16** remote host recovery — S31→S30→S26; binding/fencing. Spec p.021.

## Wave 5 — Dogfood, CI and release gates

**Depends on:** W01–W16. **Exit:** both hosts pass the same behavioral fixtures; A01–A40 pass or are explicitly blocked with evidence; visual baselines are deliberately re-pinned; no fake green states remain.

- **5.1 Acceptance harness driver.** Execute all 40 A scenarios against one daemon and both hosts, with deterministic fixtures and field-level assertions. Responsible: new `tests/ui-acceptance/` (or extend `apps/vscode/scripts/selftest.mjs` and JetBrains `FrontendSmoke.kt`), `docs/specs/edition5/acceptance.md`. Spec pp.173–176.
- **5.2 Visual/baseline gate.** Add the new screens to VS Code 240/320/480/800 baselines and JetBrains offscreen platform digests; require empty/large/invalid/stale, 200% text, keyboard/IME, high contrast and a running stream. Responsible: `apps/vscode/scripts/render-webview.mjs:52`, `apps/jetbrains/compile-and-smoke.sh`, `apps/jetbrains/.../visual-baselines.json`. Spec pp.023, 036.
- **5.3 Host matrix.** Both hosts run the same semantic fixtures and report per-host pass/fail; no host is certified by the other's results. Responsible: `apps/jetbrains/.../JetBrainsHostMatrixSmoke.kt`, VS Code selftest. Spec p.024.
- **5.4 Fault/security tests.** Crash mid-operation, corrupt/truncated storage, duplicate replay, out-of-order events, stale PID/handle, wrong session/generation, oversize MCP/hook/attachment, path traversal, secret redaction. Responsible: `apps/vscode/scripts/selftest.mjs`, `crates/*` tests, `tests/static-authority`. Spec pp.010–012, 036–037; repo `AGENTS.md` commandment 3.
- **5.5 Dogfood worktree flow.** Add a repeatable script that creates/binds a worktree, runs W01–W16 on both hosts against a real daemon, captures screenshots/transcripts and asserts receipts; no demo-only path. Responsible: new `scripts/dogfood-edition5.sh` (or `.woodpecker` lane), `apps/vscode`/`apps/jetbrains` harnesses. Spec pp.019–021, 024, 031; **PDF does not name a dogfood script**—this is a repository implementation requirement derived from the binding/worktree rules.
- **5.6 CI wiring.** Keep the existing commands green: `cargo fmt --check`, `cargo check --workspace`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-ci`; add the acceptance/visual/dogfood lanes only after they are deterministic and bounded. Responsible: `.woodpecker/`, `AGENTS.md`. Spec pp.024–025, 036–037.
- **5.7 Release evidence.** Publish the A01–A40 matrix, host matrix, baseline re-pin record, performance artifact and known limitations. The PDF requires honest unavailable states, not omitted features (p.024). Responsible: release notes + `target/certification/` artifacts.

## Recommended order (summary)

1. **Foundation:** 1.1 configuration → 1.2 model authority → 1.4 authority/evaluator → 1.3 context → 1.5 operations → 1.6 budget → 1.7 proof → 1.8 diagnostics.
2. **Controls:** 2.1 settings shell → 2.2 providers/models → 2.3 tuning → 2.4 context/compaction → 2.5 permissions → 2.6 budgets → 2.7 MCP → 2.8 skills/hooks → 2.9 rules/memory → 2.10 agents.
3. **Screens:** S01/S02/S03/S05 first (the admission spine), then S08–S14, then S15–S18, then S19–S32.
4. **Flows:** W01–W03 (model/provider), W05–W07 (context/compaction/authority), W08–W11 (customization/agents), W12–W16 (execution/proof/recovery).
5. **Dogfood/CI:** A01–A40, visual baselines, host matrix, fault/security, dogfood worktree, release evidence.

## Definition of done per control (p.176)

A row is complete only when all are true: default visible; validation rejects invalid input before persistence/dispatch; apply is transactional and has a served apply state; ownership is fenced to the captured session/worktree/daemon generation; empty/loading/stale/error/unavailable states are honest; recovery is safe and preserves drafts; keyboard/help/source are present; both hosts pass the same fixture; the matching A scenario and field-level checks pass. Rendering a widget is not completion.

## Explicit ambiguities / risks

- The PDF's `Build_Map.csv` and `implementation/*` files are not in this repository; `controls.json` is the authoritative reconstructed mapping.
- Per-control host applicability is not stated separately. The PDF says every screen/control is rendered for both hosts (pp.101–108); host-only notes are limited to controls whose behavior explicitly mentions a host (O-07, D-09, D-11).
- Per-control empty/error/disabled copy is not specified for every control; `controls.json` records the global rules (p.013) and the control-specific behavior. Do not invent copy; add it with the screen implementation and visual review.
- The PDF does not define a named dogfood worktree script or numeric performance SLOs. The backlog derives the dogfood driver from pp.019–021/024/031 and uses the repository's existing performance harness rather than inventing certifications.
- `crates/cli/src/tools_market.rs` is the Faktor Acquire commerce source market, **not** a skills marketplace. Skill discovery lives in `crates/instructions` and requires new management routes.
