# Edition 8 implementation backlog

Source: `Faktor_IDE_UI_UX_3.pdf` Revision 8 (17 Oct 2026), product/behaviour pp.197–225, construction pp.226–238, outlines pp.239–256, controls pp.257–294, screens pp.20–180, workflows pp.183–186, acceptance pp.343–345. Status is generated from `functional-evidence.json`; it is not pixel acceptance.

## Current-state snapshot

<!-- edition8-status-counts:begin -->
- **200 controls:** 199 `exists`, 1 `partial`, 0 `missing`. Generated from `functional-evidence.json` by `node scripts/edition8/ledger.mjs --sync`; do not edit this block by hand.
<!-- edition8-status-counts:end -->
- **72 screens:** both hosts carry the Rev8 registry (`apps/vscode/src/generated/edition8Registry.ts`, `Edition8CatalogData.kt`), generated from `registry.json` with 144 host contracts. The plate/layout contract is `screens.md`; the visual matrix gate executes all 72 screens in both hosts.
- **16 flows:** W01–W16 are executable in both hosts with five cells (pointer, keyboard, screen-reader automation, compact/zoom, recovery) in `tests/edition8/workflows.mjs`; the isolated dogfood drives the real daemon through every flow family.
- **40 acceptance scenarios:** A01–A40 are executable in both hosts with forbidden-state scans and field checks (`apps/vscode/scripts/edition8-acceptance.mjs`, the JetBrains smoke, and the daemon production-wiring probes). The PDF's "none executed" statement (p.343) describes the spec handoff, not this tree.
- **Backend:** every control's daemon seam (route, configuration descriptor or explicit host-owned surface) and the live receipt that proves it are recorded per host in `functional-evidence.json`; `node scripts/edition8/ledger.mjs --check` normalizes aliases and fails on a missing seam. No route-family gap remains.

## Per-category status

<!-- edition8-status-table:begin -->
| Category | exists | partial | missing | Main blocker |
|---|---:|---:|---:|---|
| M Models and routing | 14 | 0 | 0 | None. |
| P Providers, credentials and local inference | 15 | 0 | 0 | None. |
| G Reasoning and generation | 14 | 0 | 0 | None. |
| A Agent presets and delegation | 12 | 0 | 0 | None. |
| C Context, attachments and retrieval | 14 | 0 | 0 | None. |
| K Compaction, memory and efficiency | 12 | 0 | 0 | None. |
| I Instructions, rules and prompt composition | 11 | 0 | 0 | None. |
| Q Permissions, autonomy and sandbox | 14 | 0 | 0 | None. |
| T Tool catalog and MCP | 14 | 1 | 0 | vscode: typed unavailable: Streamable HTTP is not negotiated by this daemon build; stdio is the implemented transport and no HTTP/OAuth state is claimed. jetbrains: the live receipt read the provider collection, not an MCP transport action; remote transport is not exercised. daemon: the MCP save route serves stdio; no streamable-HTTP destination/auth/reconnect path is negotiated. |
| S Skills, commands and hooks | 14 | 0 | 0 | None. |
| X Terminal, browser and language tooling | 13 | 0 | 0 | None. |
| B Budgets, retries and reliability | 12 | 0 | 0 | None. |
| V Changes, checkpoints, verification and finish | 12 | 0 | 0 | None. |
| D Sessions, displays and accessibility | 15 | 0 | 0 | None. |
| O Privacy, diagnostics, optional cloud and distribution | 13 | 0 | 0 | None. |
<!-- edition8-status-table:end -->

## Wave 0 — Freeze and regenerate (no UI effects)

**Depends on:** none. **Exit:** Rev8 IDs/counts are immutable; fixtures and visual baselines are agreed; no runtime behavior changes.

### 0.1 Freeze Rev8 artifacts
Publish `controls.json`, `screens.md`, `flows.md`, `build-map.md`, `build-map.csv`, `delta.md`. Verify 200 controls, 72 screens, 16 flows, 40 A scenarios.
**Files/areas:** `docs/specs/edition8/`.
**Spec:** pp.20–180, 257–294.

### 0.2 Add the Rev8 screen registry
Create a 72-screen registry (or extend the Edition 5 generator) with screen IDs, titles, groups, host surfaces, primary actions and control footers; keep the Edition 5 registry until the new one is authoritative.
**Files/areas:** `scripts/generate-edition5.mjs`, `apps/vscode/src/generated/edition5Registry.ts`, `apps/jetbrains/.../Edition5CatalogData.kt`.
**Spec:** pp.20–180.

### 0.3 Freeze state vocabulary and host metrics
Publish the 4-step spacing tokens, 24/36/156/20/8/16/24 metrics, status vocabulary and host typography. Add `spacing-tokens.json` and `ux-fixtures.json` that the PDF names but does not include.
**Files/areas:** `apps/vscode/media/chat.css`, `apps/vscode/src/edition5/styles.ts`, `apps/jetbrains/.../Edition5Panels.kt`.
**Spec:** pp.8–19, 236, 359–382.

### 0.4 Build dual-host semantic fixtures
Create deterministic empty/loading/stale/error/disabled/large/narrow fixtures for every screen family; include 240/320/480/720+ widths, 200% text, high contrast, long stream.
**Files/areas:** `apps/vscode/scripts/render-webview.mjs`, `apps/jetbrains/.../JetBrainsHostMatrixSmoke.kt`.
**Spec:** pp.210, 225, 236–238.

## Wave 1 — Native shell, setup/admission and existing workflows

**Depends on:** Wave 0. **Exit:** Work/Inspect/History, setup, admission, recovery and proof shells match the Rev8 screen contracts; every E control has its Rev8 states in both hosts.

### 1.1 Native shell and IA
Replace the single cockpit chrome with the Rev8 Work / Inspect / History destinations, one status line per header, pinned attention, composer/config/usage order, and host-native chrome.
**Files/areas:** `apps/vscode/src/extension.ts`, `webview.ts`, `media/chat.js`, `media/chat.css`; `apps/jetbrains/.../FaktorChatPanel.kt`, `FaktorToolWindowFactory.kt`, `UiWork.kt`.
**Spec:** pp.40–52, 197–211.

### 1.2 Setup and start
Implement S34/S35/S57: first-open checklist, explicit executable trust with checksum/publisher, shell isolation with platform capabilities, draft preserved.
**Files/areas:** `apps/vscode/src/preflight.ts`, `readiness.ts`, `daemon.ts`; `apps/jetbrains/.../SettingsPanel.kt`.
**Spec:** pp.20–28.

### 1.3 Admission and attachments
Implement S36/S33/S37/S38/S39: ready composer, task options, attachment admission, pending start, ambiguous-start reconciliation.
**Files/areas:** `apps/vscode/src/taskStart.ts`, `lifecycleFence.ts`, `webview.ts`; `apps/jetbrains/.../FaktorChatPanel.kt`, `Edition5Panels.kt`.
**Spec:** pp.29–38.

### 1.4 Recovery and stop
Implement S70/S49: scoped stop (run/response/process) and cancelled-run kept-work summary.
**Files/areas:** `apps/vscode/src/extension.ts`, `lifecycleFence.ts`; `apps/jetbrains/.../FaktorChatPanel.kt`, `BlockersPanel.kt`.
**Spec:** pp.51–52, 79–80.

### 1.5 Inspect overview and plan
Implement S41/S42: four completion boundaries, dependency-ordered plan, selected-step evidence, no planned checkmark treated as proof.
**Files/areas:** `apps/jetbrains/.../OverviewPanel.kt`, `TaskTreeModel.kt`; `apps/vscode/src/cockpit.ts`, `webview.ts`.
**Spec:** pp.55–58.

### 1.6 Native diff and review
Implement S24/S43: host diff, immutable base/candidate, hunk/claim navigation, review comments.
**Files/areas:** `apps/vscode/src/changeReview.ts`, `media/chat.js`; `apps/jetbrains/.../FaktorChatPanel.kt`.
**Spec:** pp.59–62.

### 1.7 Verification, evidence and outcome
Implement S45/S46/S47/S32/S65: criterion list, evidence excerpts, proof identity mismatch, four-boundary outcome and verified outcome.
**Files/areas:** `apps/vscode/src/evidenceDocuments.ts`, `outcomes.ts`; `apps/jetbrains/.../OverviewPanel.kt`, `TaskTreeModel.kt`; routes `/native/tasks/{id}/proof`, `/completion-steps`.
**Spec:** pp.63–76.

### 1.8 Finish ladder
Implement S48: Local/Commit/Push/PR receipts, blocked native step and Configure Remote action.
**Files/areas:** `apps/vscode/src/changeReview.ts`, `outcomes.ts`; `apps/jetbrains/.../OverviewPanel.kt`.
**Spec:** pp.71–72.

### 1.9 History and safe switch
Implement S26/S61/S71/S72: title history, empty/no-match/failed-read states, full-transcript Find, draft-preserving conversation switch.
**Files/areas:** `apps/jetbrains/.../HistoryPanel.kt`, `FaktorChatPanel.kt`; `apps/vscode/src/sessionSwitch.ts`, `media/chat.js`.
**Spec:** pp.91–99.

### 1.10 Terminal and agents
Keep the existing terminal/agent controls but place them in the Rev8 S19/S23/S50 surfaces with owned-process and shared-budget presentation.
**Files/areas:** `apps/vscode/src/extension.ts`, `pixelAgents.ts`; `apps/jetbrains/.../TerminalPanel.kt`, `TaskTreeModel.kt`.
**Spec:** pp.82–85, 156–157.

## Wave 2 — Configuration authority, models, providers and policy

**Depends on:** Wave 1 shell (S02–S07, S13, S21 can develop against fixtures earlier). **Exit:** config snapshot/patch, provider CRUD/vault/test, capability descriptors, parameter mapping/effective receipt, presets and persistent per-tool policy are live in both hosts.

### 2.1 Configuration service
Make descriptors/effective snapshot/atomic patch the only write path; persist expected revision, per-field apply state and secret references; wire `/native/config/*` to both hosts.
**Files/areas:** `crates/cli/src/config/*`, `crates/server/src/native/foundation.rs`, `apps/vscode/src/edition5/commands.ts`, `apps/jetbrains/.../Edition5ConfigSource.kt`.
**Spec:** pp.226–228, 239–240.

### 2.2 Provider lifecycle and vault
Implement connection CRUD, separate test stages, credential rotation/delete and health; keep private-LAN/proxy/TLS missing until policy exists.
**Files/areas:** `crates/server/src/native/foundation.rs`, `crates/cli/src/config/provider.rs`; panels `Edition5Panels.kt`, VS Code settings.
**Spec:** pp.108–113; controls P-01–P-15.

### 2.3 Model capability and tuning
Implement exact-model capability sheet, favorites/recents, context override, adapter parameter descriptors, effective-request receipt and comparison.
**Files/areas:** `crates/router`, `crates/provider`, `/native/model-parameters*`, `/native/call-receipts`; `ModelPickerDialog.kt`, `Edition5Panels.kt`.
**Spec:** pp.108–121; controls M/G.

### 2.4 Permission policy/simulator/audit
Replace one-shot-only approval with versioned rules, test-a-rule preview, expiry/revocation and decision audit; keep one-shot approval exact.
**Files/areas:** `crates/server/src/permission.rs`, `/native/permissions/*`; `ApprovalCards.kt`, `Edition5Panels.kt`.
**Spec:** pp.142–143; controls Q.

### 2.5 Agent presets and envelopes
Make Code/Ask/Plan/Debug/Review enforced envelopes with model/tool/delegation/budget limits; custom agents cannot exceed parent authority.
**Files/areas:** `crates/orchestrator`, `/native/agents*`; `Edition5Panels.kt`, `FaktorChatPanel.kt`.
**Spec:** pp.118–121; controls A.

### 2.6 Budget revision and reliability
Expose active-run budget revision, retry classes/backoff/loop guards and uncertain-spend reconciliation.
**Files/areas:** `crates/orchestrator`, `/native/session/{id}/usage`, `/native/agents/{child}/budget`; `Edition5Panels.kt`.
**Spec:** pp.161–164; controls B.

## Wave 3 — Context, instructions, memory and customization

**Depends on:** Wave 1 shell and Wave 2 config/model authority. **Exit:** attributed context/compaction, memory/rules, MCP stdio lifecycle, hook tests and skills/commands are real, bounded and recoverable.

### 3.1 Context meter and manifest
Replace nullable projection with a versioned context read; show sent/reserved/capacity, categories, source identity and null-as-unavailable.
**Files/areas:** `crates/context`, `/native/session/{id}/context/*`, `/native/context/*`; `Edition5Panels.kt`, `media/chat.js`.
**Spec:** pp.123–130; controls C-01–C-06.

### 3.2 Add-context picker and provenance
Implement exact file/range/diagnostic/terminal/diff/URL/prior-chat picking, per-source admission and preview.
**Files/areas:** `apps/vscode/src/extension.ts`, `webview.ts`; `Edition5Panels.kt`; attachment routes.
**Spec:** pp.127–128; controls C-05/C-09/C-13.

### 3.3 Compaction operation
Make compaction a durable operation with plan, minimum reduction invariant, retained obligations and post-compaction inspection.
**Files/areas:** `crates/context`, `/native/session/{id}/context/compact`; `Edition5Panels.kt`.
**Spec:** pp.129–130; controls K-01–K-04.

### 3.4 Memory and retention
Expose versioned personal/project memory, source/date, delete-future-retrieval semantics and retention/cleanup.
**Files/areas:** `crates/memory`, `/native/memory*`; `Edition5Panels.kt`.
**Spec:** pp.137–140; controls K-10/K-11.

### 3.5 Instructions and precedence
Show origin/scope/enablement, merged preview, conflicts, managed locks and version binding; import rules without broadening authority.
**Files/areas:** `crates/instructions`, `/native/config/*`; `Edition5Panels.kt`.
**Spec:** pp.133–136; controls I.

### 3.6 MCP lifecycle
Complete stdio CRUD, environment references, start/stop, discovery, health, per-tool allow/deny, logs and remove; remote/OAuth stays unavailable.
**Files/areas:** `crates/mcp`, `/native/mcp/servers*`; `Edition5Panels.kt`.
**Spec:** pp.144–149; controls T-01–T-15.

### 3.7 Skills, commands and hooks
Complete skill install/update/remove/trust/preview, command type disclosure and hook event/args/env/bounds/test.
**Files/areas:** `crates/instructions`, `crates/hooks`, `/native/skills*`, `/native/hooks*`; `Edition5Panels.kt`.
**Spec:** pp.150–155; controls S.

## Wave 4 — Advanced recovery, usage and optional services

**Depends on:** Waves 1–3. **Exit:** content-aware restore/fork/redo, safe parallel switching, remote binding, browser adapter, deeper outcomes, retention/telemetry/import-export are explicit and reversible.

### 4.1 Restore, fork and partial landing
Implement restore preview, content coverage, fork lineage, redo and hunk/file acceptance; never overwrite user edits or claim universal undo.
**Files/areas:** `crates/session/src/checkpoints.rs`, `/native/session/{id}/fork`, checkpoints/restore; `Edition5Panels.kt`.
**Spec:** pp.77–78; controls V-03/V-05/V-06.

### 4.2 Remote workspace and protocol recovery
Show editor/workspace/model/vault locations, fence callbacks by generation, preserve unknown effects and recover protocol mismatch explicitly.
**Files/areas:** `apps/vscode/src/workspaceBinding.ts`, `lifecycleFence.ts`; `apps/jetbrains/.../Edition5Panels.kt`; `/native/health`, `/native/ready`.
**Spec:** pp.175–178; controls X-13, O-02/O-11.

### 4.3 Parallel conversations and worktrees
Bound parallel sessions/worktrees, per-session drafts, owned tabs and stale-callback fencing.
**Files/areas:** `apps/vscode/src/sessionSwitch.ts`; `/native/sessions/parallel`; `HistoryPanel.kt`.
**Spec:** pp.96–99; control D-15.

### 4.4 Browser adapter
Do not fake it: add a real advertised adapter with account/machine/network scope, URL validation, preview, screenshot and console/network observations.
**Files/areas:** `crates/server/src/native/foundation_ext.rs` (`/native/browser/status`, `validate-url`); new adapter seam; `Edition5Panels.kt`.
**Spec:** pp.158–159; controls X-06–X-09.

### 4.5 Usage outcomes and cohorts
Implement recorded outcome cohort, verified-change denominator, failed/cancelled spend, local vs hosted routes, and unavailable-not-zero semantics.
**Files/areas:** `apps/vscode/src/outcomes.ts`, `/native/outcomes`, `/native/usage`; `Edition5Panels.kt`.
**Spec:** pp.101–106; controls S-53/S-54/S-27.

### 4.6 Settings provenance, conflict and import/export
Add effective-settings search, scope/lock provenance, revision conflict resolution, retention/cleanup and configuration import/export.
**Files/areas:** `apps/vscode/package.json` settings, `Edition5Panels.kt`, `/native/config/{search,export,import}`.
**Spec:** pp.165–170; controls O-01/O-09/O-12.

### 4.7 Optional account, telemetry and updater truth
Keep local coding independent; make control-plane account optional/write-only; telemetry off by default; updater status/signing truth in diagnostics.
**Files/areas:** `apps/vscode/src/controlPlaneAuth.ts`, `/native/updater/*`, `/native/identity`; `Edition5Panels.kt`.
**Spec:** pp.171–174; controls O-07/O-08/O-10/O-13.

## Wave 5 — Evidence and completion (parallel with 2–4, never skipped)

**Depends on:** the relevant surface. **Exit:** every claim in the UI is backed by a receipt; the four completion boundaries cannot be collapsed.

### 5.1 Wire the 200 controls to live routes
Replace Edition 5 proposed route constants with the actual daemon routes or add compatible routes; fail closed on shape mismatch. Current evidence: `Edition5ConfigSource.kt` constants such as `/native/config/effective`, `/native/context/export`, `/native/browser/connect`, `/native/budget/revision`; actual route table in `crates/server/src/api/lifecycle.rs`.
**Files/areas:** `apps/vscode/src/edition5/commands.ts`, `Edition5ConfigSource.kt`, `crates/server/src/api/lifecycle.rs`.
**Spec:** pp.226–238, 257–294.

### 5.2 Remove fabricated/placeholder effectiveness
Any control rendered in the Edition 5 control center without a live effect must show a typed unavailable reason, never a fake effective state.
**Files/areas:** `apps/vscode/src/edition5/host.ts`, `view.ts`, `Edition5Panels.kt`.
**Spec:** pp.13, 222, 226.

### 5.3 Outcome proof cross-check
Ensure acceptance, claims, integration identity and finish receipts are read from durable proof, not chat completion; keep historical proof separate from current candidate.
**Files/areas:** `crates/server/src/native/*` proof routes; `OverviewPanel.kt`, `outcomes.ts`.
**Spec:** pp.63–76, 343–345.

## Top 12 current-state gaps (ordered by dependency)

1. **No Rev8 72-screen registry or native IA.** The tree has the 32-screen Edition 5 registry; S33–S72 have no dedicated screen/layout implementation.
   - Areas: `apps/vscode/src/generated/edition5Registry.ts`, `Edition5CatalogData.kt`, `extension.ts`, `FaktorChatPanel.kt`.
   - Spec: pp.20–180.
2. **Native fidelity is still Edition 5.** Host-specific typography, button order, metrics, chrome, validation and surfaces are not applied across the 72 screens.
   - Areas: `media/chat.css`, `Edition5Panels.kt`, `SettingsPanel.kt`, `FaktorConfigurable.kt`.
   - Spec: pp.8–19, 359–382.
3. **Setup/start has no Rev8 surface.** S34/S35/S57 first-open, executable trust and shell isolation are not dedicated screens; draft retention is not proved.
   - Areas: `preflight.ts`, `readiness.ts`, `daemon.ts`, `SettingsPanel.kt`.
   - Spec: pp.20–28.
4. **Admission/recovery has no Rev8 surface.** S36–S39, S70 and S72 are not implemented as specified; pending/ambiguous/stop/switch states rely on old chat behavior.
   - Areas: `taskStart.ts`, `lifecycleFence.ts`, `sessionSwitch.ts`, `FaktorChatPanel.kt`.
   - Spec: pp.29–38, 51–52, 98–99.
5. **Work is not the Rev8 cockpit.** Pinned attention, transcript, composer, quiet config row and one status line are not the current layout/priority.
   - Areas: `media/chat.js`, `media/chat.css`, `UiWork.kt`, `FaktorChatPanel.kt`.
   - Spec: pp.40–52.
6. **Inspect/proof surfaces are incomplete.** No S41/S42/S45/S46/S47/S65 full surfaces; evidence/finish identity is spread across old panels.
   - Areas: `OverviewPanel.kt`, `changeReview.ts`, `evidenceDocuments.ts`, `outcomes.ts`.
   - Spec: pp.55–76.
7. **Settings provenance/search/conflict missing.** No S60/S64 effective-source/lock/conflict UI; native settings remain category-local.
   - Areas: `SettingsPanel.kt`, `FaktorConfigurable.kt`, `edition5/view.ts`, `/native/config/*`.
   - Spec: pp.165–170.
8. **Models/providers/tuning not Rev8.** S02–S05/S63 remain a picker plus generic settings; no exact capability sheet, separate test stages or effective-request screen.
   - Areas: `ModelPickerDialog.kt`, `Edition5Panels.kt`, `edition5/commands.ts`, `/native/model-parameters*`.
   - Spec: pp.108–121.
9. **Context/compaction/memory/instructions not Rev8.** S08–S12/S62/S66/S69 lack the specified surfaces and state distinctions; null usage/estimate bases remain easy to conflate.
   - Areas: `Edition5Panels.kt`, `crates/context`, `crates/memory`, `/native/context/*`.
   - Spec: pp.123–140.
10. **Tools/MCP/skills/hooks/terminal/browser not Rev8.** S13/S15–S20/S67/S68 are split across generic panels; remote/OAuth/browser/logs/resources are still absent.
   - Areas: `Edition5Panels.kt`, `foundation_ext.rs`, `TerminalPanel.kt`.
   - Spec: pp.142–159.
11. **Usage/outcomes not Rev8.** S27/S53/S54 lack the recorded-outcome cohort, verified-change denominator and unavailable-not-zero display.
   - Areas: `outcomes.ts`, `/native/outcomes`, `/native/usage`, `Edition5Panels.kt`.
   - Spec: pp.101–106.
12. **UI command routes do not match the daemon.** Edition 5 UI constants point at proposed paths while the daemon exposes different route names/shapes; controls can render as effective without a live effect.
   - Areas: `apps/vscode/src/edition5/commands.ts`, `Edition5ConfigSource.kt`, `crates/server/src/api/lifecycle.rs`.
   - Spec: pp.226–238, 257–294.

## Dependencies and release rule

- Do not ship a screen whose values/effects/recovery are simulated. The PDF is explicit: a screen is ready only when values, effects and recovery are real (p.225).
- Backend-first for N controls; host-only work may proceed against versioned fixtures, but no mock becomes the production source of model usage, permissions, saved settings or completion (p.226).
- Keep the 200 control IDs and 40 A IDs stable; do not renumber screens around implementation convenience. Rev8 screen IDs are contract identities.
