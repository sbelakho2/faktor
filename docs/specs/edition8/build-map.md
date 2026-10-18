# Edition 8 build map and construction plan

Source: `Faktor_IDE_UI_UX_3.pdf` Revision 8 (17 Oct 2026), required backend/native construction pp.226–238, implementation outlines pp.239–256, control contracts pp.257–294, construction waves p.225. This is a **reconstructed** build map: the PDF requires a handoff `Build_Map.csv`, Rust/TypeScript/Kotlin/CSS outlines, `spacing-tokens.json` and `ux-fixtures.json`, but those companion files are not inside the PDF. `build-map.csv` reconstructs one row per control from `controls.json`.

## 1. Counts and provenance

- **200 control rows** (all 200 IDs, no additions/removals from Edition 5).
- **72 screens** and **16 workflows**; **40 acceptance scenarios** A01–A40.
- **10 backend packages** BK01–BK10, **3 host/visual packages** UI01–UI03.
- Control rows carry `reconstructed=yes`; the PDF does not contain the CSV itself.
- The 200 control contracts are unchanged in default, scope, apply timing and E/R/N/H classification from Edition 5; only screen mapping and visual/native presentation changed (see `delta.md`).

## 2. Build packages and ownership (pp.226–238)

| Package | Owns | Must not own | Pages |
|---|---|---|---|
| **BK01 Configuration** | Descriptors, effective snapshots, atomic patches, secret references, per-field apply state | Provider transport, UI precedence | pp.227–228, 239–240 |
| **BK02 Models/providers** | Provider lifecycle, capability/parameter mapping, normalized call receipts | Secret plaintext, UI policy | pp.228–229, 240 |
| **BK03 Customization** | Trusted versioned presets/rules/memory/skills, deterministic instruction manifests | Tool execution, permission grants | pp.229–230, 243–244 |
| **BK04 Context** | Exact context/accounting, source identity, compaction | UI estimate as measurement | pp.229–230, 240–241 |
| **BK05 Authority** | Effective policy, simulation, enforcement, truthful sandbox status | Persistent approval waiter only (BK08) | pp.230–231, 241 |
| **BK06 Tools/MCP/hooks** | Tool/MCP/hook lifecycle, auth, bounded process effects | Permission broadening from install/enable | pp.230–231, 242 |
| **BK07 Budgets/reliability** | Shared money/token reservations, routing/retries/efficiency reads and changes | Task admission identity | pp.232–233, 244–245 |
| **BK08 Sessions/recovery** | Session/queue/process/remote identity, generation fencing | Product policy/ledger decisions | pp.231–232, 245 |
| **BK09 Review/proof** | Review/restore/fork/evidence/finish | Host diff rendering | pp.232–233, 246 |
| **BK10 Diagnostics/optional services** | Runtime trust, readiness, diagnostics, updater/enterprise/cloud/worker services | Local preference overriding managed policy | pp.231, 247 |
| **UI01 VS Code adapter** | Host presentation/controller over shared semantic fixtures | Policy/ledger decisions in the webview | pp.248, 250 |
| **UI02 JetBrains adapter** | Host presentation/controller over shared semantic fixtures | Policy/ledger decisions in Swing events | pp.249–250 |
| **UI03 Visual system** | Spacing/typography/overflow tokens and visual acceptance gate | Feature semantics | p.236, p.250 |

Ambiguity: the PDF never states a single UI01/UI02 mapping table; the host mapping is inferred from the outline languages and host sections (VS Code TypeScript, JetBrains Kotlin).

## 3. Seam contracts (pp.239–256)

| Reference seam | Core decision | Transaction / acknowledgement | Current repository area |
|---|---|---|---|
| `contracts/configuration.rs` | Typed patch decision from current revision + descriptors + policy + capability snapshot; unknown keys fail. | Compare expected revision inside the store transaction; persist all or none; saved-but-not-applied is first-class. | `crates/cli/src/config/*`; `/native/config/{descriptors,snapshot,preview,commit,rollback,runtime-ack,search,export,import}` |
| `contracts/generation.rs` | Adapter-supported parameter descriptors; requested vs effective vs unsupported fields. | Normalized call receipt at dispatch; unknown parameters rejected before dispatch. | `/native/model-parameters`, `/native/model-parameters/compile`, `/native/call-receipts` |
| `contracts/context.rs` | Source identity (URI/range/version/hash), versioned manifest, one-basis accounting. | Context revision; compaction is its own durable operation with a minimum reduction. | `crates/context/src/budget.rs`; `/native/session/{id}/context/{preview,compact}`, `/native/context/manifest(s)` |
| `contracts/authority.rs` | One effective policy decision for preview and dispatch; managed denies; sandbox truth. | Persistent rule revision; one-shot waiter generation; revocation acknowledged. | `crates/server/src/permission.rs`; `/native/permissions/{policy,preview,rules,audit}` |
| `contracts/tools.rs` | Tool identity = server instance + stable tool name; effect class; bounded stdio spec. | Explicit start/restart acknowledgement; remote transport only after negotiation. | `crates/cli/src/config/mcp.rs`; `/native/mcp/servers*` |
| `contracts/customization.rs` | Versioned content for presets/rules/memory/skills; source/version/scope. | Install/update/delete receipt; prior known-good version retained. | `crates/instructions/src/lib.rs`; `crates/memory/src/lib.rs`; `/native/{skills,hooks,memory}*` |
| `contracts/budget.rs` | Integer micro-USD; shared parent/child reservation; eligible attempted spend. | Admission receipt UUID + body hash; reservation/settlement/refund. | `crates/orchestrator`; `/native/session/{id}/usage`, `/native/agents/{child}/budget` |
| `contracts/operations.rs` | Binding tuple (session, worktree, daemon, generation); owned queue/process identity. | Durable admission/finish receipt; `effect_status = unknown` on uncertain effects. | `apps/vscode/src/lifecycleFence.ts`; `/native/session/{id}/task-runs*`, terminal routes |
| `contracts/review.rs` | Candidate/proof identity; content-aware restore; expected current hash. | CAS transaction; fork and restore are separate. | `crates/session/src/checkpoints.rs`; `/native/session/{id}/fork`, checkpoints/restore, `/native/tasks/{id}/proof` |
| `contracts/diagnostics.rs` | Allowlisted runtime status; redacted bounded export. | Explicit start/export acknowledgement; checksum ≠ signature verification. | `/native/health`, `/native/ready`, `/native/diagnostics/{preview,export}` |
| `contracts/inspection.rs` | Four completion boundaries: acceptance, claims, integration, finish. | Durable proof/landing receipts; pending stays pending. | `/native/tasks/{id}/proof`, `/native/tasks/{id}/completion-steps`, `/native/session/{id}/verification` |
| `hosts/vscode/controller.ts` | Host adapter; selection/focus/draft restoration; generation fence. | Host action receipts; no policy/ledger logic. | `apps/vscode/src/extension.ts`, `webview.ts`, `edition5/*` |
| `hosts/vscode/layout.css` | `--s1..--s8` spacing, label/help/error slots, overflow rules. | Visual acceptance gate. | `apps/vscode/media/chat.css`, `apps/vscode/src/edition5/styles.ts` |
| `hosts/vscode/interaction.ts` | Quick Pick/settings/diff/terminal interaction mapping. | Escape restores invoker; focus returns to exact row. | `apps/vscode/src/extension.ts`, `edition5/commands.ts` |
| `hosts/vscode/immutable-diff.ts` | Read-only base/candidate diff identity. | No mutation through the diff surface. | `apps/vscode/src/changeReview.ts` |
| `hosts/jetbrains/FaktorController.kt` | Native components; network/process effects off the EDT. | Host action acknowledgements; no ledger logic. | `apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/*` |
| `hosts/jetbrains/FaktorForm.kt` | JBUI/UIManager metrics, focus, validation and dialog order. | OK/Apply/Cancel semantics per Configurable. | `FaktorConfigurable.kt`, `SettingsPanel.kt`, `Edition5Panels.kt` |

## 4. Spacing, type and visual tokens (pp.236, 359–382)

```css
/* 4 logical-pixel base; wide surface */
--space-1: 4px;   /* inline minimum */
--space-2: 8px;   /* control/helper gap */
--space-4: 16px;  /* complete field row / between groups */
--space-6: 24px;  /* section gap / wide inset */
--label-column: 156px;
--column-gap: 20px;
--control-height: 36px;
--helper-gap: 8px;
--row-gap: 16px;
--section-gap: 24px;
/* compact host surfaces may adapt insets, but not move controls off-grid */
--target-min: 24px;      /* buttons 26/28px */
--composer-action: 36px; /* where host metrics permit */
```

- Stack labels above values below **560 logical px**; test 240/320/480/720+ widths, 100/150/200% text/UI scaling and short tool-window heights (pp.210, 236).
- Body line-height 1.4–1.5; labels 1.25–1.35; never clip a fixed single-line box (p.236).
- Identity palette: Ink `#08110C`, Forest `#143D2B`, Paper `#F3F5EF`, Sage `#92AA99`, Ochre `#D9BA75`, Ochre Deep `#72541E`; Ochre/Paper 1.7:1 decorative only; normal text target 4.5:1, essential graphics 3:1 (pp.372–373).
- Type roles: Barlow Semi Condensed 600/700 display (56–108 px), Barlow Semi Condensed section (38–66 px), Barlow reading/controls (19–25 px), IBM Plex Mono metadata (10–14 px). Native IDE surfaces inherit host fonts; do not load marketing fonts into the widget (p.374).

## 5. Fixtures (pp.225, 236–238)

Every screen must render deterministic semantic fixtures in both hosts:

- **Global:** empty, loading (bounded/cancellable), stale (age + last coherent snapshot), typed error (cause/scope/recovery), disabled (missing prerequisite), 200% text, 240/320/480/720+ widths, keyboard-only, screen reader, high contrast, long-running stream.
- **Setup/admission:** service not running; unselected executable/model/shell; checksum unrecorded; signature unverified; task options default/captured; attachment ready/unsupported/failed; starting duplicate suppression; ambiguous admission.
- **Work/agents:** attention card; queued next; scoped stop; child queued/active/waiting/needs-approval; shared budget saturation; comparison A pending/B recommended; coordination unread/read.
- **Inspect/proof:** not complete; pending evidence; stale proof identity; integration conflict; finish blocked at push; verified complete.
- **Context/customization:** null usage; over-capacity; compaction plan/refused/cancelled/insufficient reduction; instruction conflict; memory delete; MCP stopped/ready/auth-required/failed; hook pass/fail/timeout/malformed; skill manual/agent-selectable.
- **Settings/usage:** settings conflict; locked by organization; no verified changes (Unavailable, not $0); unknown price; no account; protocol mismatch; workspace moved; 320 px narrow Work.

## 6. Construction waves (p.225; control wave rows are reconstructed)

| Wave | Scope | Exit |
|---|---|---|
| **0 — Freeze** | Publish Rev8 artifacts; regenerate the 72-screen registry; freeze control IDs/screens/state vocabulary; create dual-host fixtures and visual baselines. | IDs/counts immutable; fixtures reviewed. |
| **1 — Native shell + existing workflows** | Work/Inspect/History shell, setup/admission/recovery, native diff, verification/outcome, terminal/agents/attention, settings search, host favorites/context pick/transcript Find. | Existing workflows W01–W16 use the Rev8 shell; every E control has Rev8 states. |
| **2 — Configuration and models** | BK01/BK02 config authority, provider CRUD/vault/test, capability descriptors, parameter mapping/effective receipt, presets, persistent per-tool policy. | Controls M/P/G/Q/B-05..B-09 exit with live routes and both-host surfaces. |
| **3 — Context and customization** | BK04/BK03 context manifest/accounting/compaction, memory/rules, MCP stdio lifecycle, hooks test, skills/commands. | C/K/I/T/S controls exit with attributed sources and bounded effects. |
| **4 — Advanced recovery** | BK09/BK08 content-aware restore/fork/redo, remote binding, parallel sessions, browser adapter, model comparison, deeper outcomes/telemetry/retention. | V-03/V-05/V-06/V-12, D-03/D-04/D-06/D-15, X-06..X-11, O-09/O-10/O-12, M-14, C-14 exit. |

The PDF says “all 200 control IDs have a construction row”; the reconstructed CSV assigns each row to the wave of its owning feature and marks `reconstructed=yes`. Wave assignment is not normative per control.

## 7. Host work summary

- **VS Code:** one Faktor view container with Work/Inspect/History; editor-sized settings tabs; native Quick Pick, diff, terminal, notifications; typed webview bridge; no trusted effects in the webview; focus/Escape restoration.
- **JetBrains:** one dockable tool window; `Settings › Tools › Faktor` Configurable; ActionSystem toolbar; native lists/popups/DialogWrapper; diff via DiffManager; console; no network/process effects on the EDT; OK/Apply/Cancel order.
- **Both:** same semantic fixtures, same status vocabulary, same acknowledgement/recovery rules; no `if provider == ...` in host code; host-only favorites/disclosure/density do not alter authority.

## 8. Verification obligations

- Each row: keyboard access, accessible name/role/state, default, allowed scope, apply timing, empty/error/disabled/loading/stale states, safe recovery, no fabricated value.
- Each screen: entry, selection, focus return, primary action, pending response, success, typed failure, scope switch (p.210).
- Each workflow: capture → effect acknowledgement → recovery; engineering outcome independent of conversation completion (pp.183–186).
- Backend: one authority per effect, strict DTOs, idempotency, durable receipts, unknown-effect handling; a mock never becomes the source of usage/permissions/settings/completion (p.226).
