# Revision 8 delta against the Edition 5 artifacts

**Revision:** Faktor / IDE experience & implementation — Revision 8, 17 October 2026, 449 pages, baseline `a89c00dbd612`. **Compared artifacts:** `docs/specs/edition5/` (Edition 5, `Faktor_IDE_UI_UX_2.pdf`). **Method:** full TOC pass, control contracts pp.257–294, screen plates/contracts pp.20–180, workflows pp.183–186, construction pp.226–238, acceptance pp.343–345, visual identity pp.359–382, then field-level diff against the Edition 5 JSON/markdown.

## 1. Count delta

| Dimension | Edition 5 | Revision 8 | Delta |
|---|---:|---:|---|
| Controls | 200 | 200 | 0 added / 0 removed |
| Screens / native hosts | 32 | 72 | +40 (S33–S72) |
| End-to-end workflows | 16 (W01–W16) | 16 (W01–W16) | 0 |
| Acceptance scenarios | 40 (A01–A40) | 40 (A01–A40) | 0 |
| Control contract pages | 51–100 (catalog + contracts) | 257–294 | relocated, not changed |
| Screen contracts | 32 | 72 | +40 |
| Build packages | BK01–BK10 + UI01–UI03 | same | 0 |

## 2. Control delta — 200 IDs, no normative field changes

- All 200 control IDs are present in both revisions: M14, P15, G14, A12, C14, K12, I11, Q14, T15, S14, X13, B12, V12, D15, O13.
- **0 additions, 0 removals.** Every default, scope, apply-timing, behaviour and E/R/N/H classification in Rev8 matches the Edition 5 control catalog (field-level diff after normalizing line-wrap artifacts).
- What changed is **where** controls appear: 101 new control-to-screen edges were added across the 40 new screens; 0 old edges were removed; 140 controls keep exactly their Edition 5 screen set. Largest new edges: S36 (7 controls), S34/S50/S33/S41 (4 each).
- The control contracts are now the explicit source for screen mapping: `controls.json` records design/contract page pairs for every screen edge and the control contract page.
- Runtime endpoints named by the controls are capability descriptions, not normative routes (p.226). In the current tree, all 506 named HTTP capabilities have a matching daemon route after path alias normalization; the remaining gap is UI wiring and Rev8 screen presentation.

## 3. Screen delta — 32 retained and redrawn, 40 added

**Retained:** S01–S32 all remain. All 32 titles are shortened/simplified in Rev8 (for example “Work: a complete agent cockpit” → “Work”; “Provider connection editor” → “Provider connection”; “Task outcome and finish proof” → “Task outcome”). The screen identity is stable; the layout and copy are redrawn on the corrected components.

**Added (40):**
- **S33** — Task options.
- **S34** — First open.
- **S35** — Service executable trust.
- **S36** — Ready conversation.
- **S37** — Attachment admission.
- **S38** — Task admission pending.
- **S39** — Ambiguous task start.
- **S40** — Answer a task question.
- **S41** — Inspect overview.
- **S42** — Plan and dependencies.
- **S43** — Native diff review.
- **S44** — Integration conflict.
- **S45** — Required verification.
- **S46** — Evidence detail.
- **S47** — Proof identity mismatch.
- **S48** — Finish actions.
- **S49** — Cancelled run.
- **S50** — Agent detail and steer.
- **S51** — Compare approaches.
- **S52** — Coordination.
- **S53** — Recorded outcome cohort.
- **S54** — Usage and quota.
- **S55** — Optional control-plane account.
- **S56** — Protocol-blocked recovery.
- **S57** — Shell isolation setup.
- **S58** — Narrow Work panel.
- **S59** — Queued messages.
- **S60** — Settings revision conflict.
- **S61** — Empty conversation history.
- **S62** — Unavailable context usage.
- **S63** — Compare exact models.
- **S64** — Settings search and provenance.
- **S65** — Verified outcome.
- **S66** — Memory deletion.
- **S67** — Hook test result.
- **S68** — Future MCP authentication.
- **S69** — Instruction precedence.
- **S70** — Scoped stop actions.
- **S71** — Find in conversation.
- **S72** — Switch conversation safely.

The new surfaces concentrate on four areas: setup/admission/recovery (S34–S39, S57, S70, S72), proof/outcome (S41–S49, S65), delegation/coordination (S50–S52), and settings/usage/ops (S53–S56, S60, S64). They make previously implicit states explicit rather than adding product scope.

## 4. Workflow and acceptance delta — none in count or obligation

- **W01–W16** remain the same 16 workflows. Rev8 adds Rev8 surfaces to their paths (for example W01 now also has setup S34/S57 and settings provenance S64; W12 has S59/S70/S72; W15 has S45/S46/S48/S65). The PDF workflow table is still a bounded outcome narrative, not a UI script.
- **A01–A40** remain the same acceptance scenario IDs. Rev8 restates them on pp.343–345; no scenario is removed. The sibling acceptance/CI artifact owns the executable mapping.
- The page references moved: workflows are now pp.183–186 (Edition 5 pp.014–021); acceptance pp.343–345 (Edition 5 p.173).

## 5. Visual/native delta — the substantive change

- **Per-host native fidelity replaces one custom chrome.** VS Code uses the activity view, editor settings tabs, Quick Pick, native diff/terminal and typed webview; JetBrains uses the dockable tool window, Configurable, popup choosers, DialogWrapper, native diff and console. Type, metrics, label style, button order, validation and surface are host-specific (pp.3, 9).
- **Optical centering fixed.** Edition 5 buttons sat 4.6 px high on average (range 4.4–4.8) across 288 R7 buttons; Rev8 trims labels to cap height/baseline and reports median +0.0 px across 1,036 labels (p.3, p.10).
- **Copy cut nearly in half.** 5,372 → 3,898 words across the 72 VS Code plates (window titles excluded); 838 changes are recorded per screen; one status line per header, help only where a decision needs it, details behind disclosures (pp.3, 15).
- **Spacing/type system made explicit.** 4 logical-pixel base; 24 px wide insets, 156 px label column, 20 px gutter, 36 px controls, 8 px helper gap, 16 px field-row gap, 24 px section gap; labels stack below 560 px; 24 px minimum targets, 26/28 px buttons, 36 px composer action; test 240/320/480/720+ widths and 100/150/200% text (pp.14, 194, 236).
- **State vocabulary made normative.** One icon+word status line; running/waiting/queued/failed/cancelled/uncertain/pending/verified distinct; empty/loading/stale/error/disabled rules; optimistic paint limited to expansion/filter/favorite/draft; Saved/Connected/Allowed/Cancelled/Restored/Verified/PR-created need their own acknowledgement (pp.13, 222).
- **Accessibility acceptance tightened.** WCAG 2.2 AA contrast (4.5:1 text, 3:1 graphics), 24 px targets, visible focus, no focus on Allow Once, Escape returns to trigger, 200% text, keyboard-only, screen reader and high-contrast tests (pp.4, 17, 210). Rev8 measured 80 token pairs in five themes: Faktor-owned tokens pass; 9 host defaults fall short and are listed for decision (p.17).
- **Visual identity v3 added as the reference for identity, not native reading surfaces:** Ink/Forest/Paper/Sage/Ochre/Ochre Deep, Barlow/Barlow Semi Condensed/IBM Plex Mono, 24 px preferred symbol, H/4 clear space, Ochre/Paper decorative only (pp.359–382). Native IDE surfaces inherit host fonts and tokens.
- **Four host themes** are built (VS Code light/dark, JetBrains light/dark, high contrast); plates show only VS Code Dark Modern and JetBrains Light (p.7).

## 6. Content retention and removals

- **Nothing was removed from the product or documentation** (p.3). The original first-report content, both source reports, identity, website and evidence are retained; R7 pages are reproduced unchanged where retained.
- What left the default interface is implementation instructions and backend vocabulary. It moved to control contracts, details disclosures, build map and backlog; for example the micro-USD conversion moved from the money-cap field to control B-02, and empty-history developer instructions moved to S61 (p.3).
- Some Edition 5 artifact assumptions are now wrong: it described 32 screens and a 25/105/70 status split. Rev8 adds 40 screens; the current functional status is generated from `functional-evidence.json` (`node scripts/edition8/ledger.mjs --check`) and is never repeated as a hand-maintained count here.

## 7. Build/artifact delta

- The PDF still requires a handoff `Build_Map.csv`, outlines, `spacing-tokens.json` and `ux-fixtures.json`, but the PDF itself contains only the construction plan and reference outlines, not those files. Edition 5 reconstructed `build-map.csv`; Rev8 reconstructs it again with 200 rows, `reconstructed=yes`, and the Rev8 screen/wave mapping.
- The 17 seam outlines are unchanged in name and intent: 11 `contracts/*.rs`, 4 VS Code host files, 2 JetBrains host files.
- The 200 control build tags are unchanged (for example M-01: BK01 UI01 UI02 UI03 BK02 BK07); the host work changed from a generic cockpit to per-screen native surfaces.

## 8. Ambiguities and caution

- The “838 changes” ledger is referenced but not enumerated in the PDF; per-screen plates show the changed copy and destination, but there is no machine-readable 838-row ledger. `screens.md` and `controls.json` record the screen/control contracts, not every copy sentence.
- The PDF says “all 200 control IDs have a construction row in Build_Map.csv” and “the catalog assigns E/R/N/H”; it does not attach a per-control wave. Wave rows in `build-map.csv` are reconstructed from pp.225–226 and marked.
- The PDF gives global loading/stale/pending state rules, not per-control copy; the per-control empty/error/disabled text comes from the catalog and may be generic for many controls.
- Screen titles are editorial; IDs are the stable contract. Use S-IDs, not titles, in implementation/tests.
- The repository now carries the Rev8 72-screen registry in both hosts (`apps/vscode/src/generated/edition8Registry.ts`, `Edition8CatalogData.kt`): 72 screens, 144 host contracts, 200 controls, 40 acceptance scenarios and 16 workflows (5 cells per host). The executable evidence is produced by `node scripts/edition8/run.mjs` and summarized in `target/certification/edition8-summary.json`; `backlog.md` records the historical gap analysis, not the current tree.

## 9. Intentional UX deviations (chat/settings overhaul)

This wave keeps the Rev8 tokens (host fonts, 4/8/12/24 spacing, 24 px targets,
host focus, status vocabulary) but changes three information-architecture
choices in response to operator review. These are deliberate deviations, not
silent registry edits: screen IDs, control IDs and native routes are unchanged.

1. **Work is conversation-first.** S01's "quiet config row" is reduced to one
   compact sticky status row (state · run · exact tokens · pending approvals)
   with Details/Stop. The goal, plan, candidate boundary and run picker move to
   Inspect › Overview › Run details. The composer stays anchored to the bottom
   and the transcript owns the remaining height (`chat.css`, `FaktorChatPanel`).
2. **Coordination is a drawer, never chat chrome.** S52 remains in
   Inspect › Agents, now as an explicitly collapsed `<details>` drawer
   (`board-card`); the Work surface contains no board, posts or board composer.
   This overrides the earlier "long transcript may keep the board beside it"
   reading of S01; evidence is `INV-CHAT-BOARD-NOT-IN-WORK`.
3. **Settings is one domain surface.** Rev8 lists settings as separate S02–S31
   screens; the shipped surface presents them as six searchable domains
   (providers/models, context/compaction, tools/MCP, permissions, billing,
   appearance) with descriptor labels/descriptions, advanced screens behind one
   disclosure per domain, human value summaries (no raw JSON), and one footer
   Apply with an explicit "autosave off" state. This keeps every S-screen and
   control reachable while removing per-screen duplicate apply actions and the
   screen-index-as-navigation model. Evidence is
   `INV-EDITION8-ONE-SETTINGS-SURFACE`.

The host-native Settings editor / Configurable remains the persistence
authority; the webview only stages a typed patch. No daemon route, control
contract or acceptance scenario is removed.
