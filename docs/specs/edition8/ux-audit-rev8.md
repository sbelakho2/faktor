# Edition 8 JetBrains UX audit (Rev8, 72 screens)

Scope: every Rev8 screen rendered by `Edition8ScreenRegistry` in `apps/jetbrains`
(Swing/Kotlin). This is a heuristic audit, not certification or soak. Raw
artifacts: `target/ux-audit/before/`, `target/ux-audit/after/`,
`target/ux-audit/evidence/`, and the verified host matrix at
`target/certification/jetbrains-host-matrix.json`.

## Outcome

| Dimension | Before (mean) | After (mean) |
|---|---:|---:|
| Clarity | 1.21 | 4.92 |
| Density | 3.44 | 4.90 |
| Consistency | 3.03 | 4.68 |
| Efficiency | 3.72 | 5.00 |
| Recovery | 4.00 | 5.00 |
| **Overall** | **3.08** | **4.90** |

Metrics at 480 px: visible interactive controls median **9.5 -> 6.0** (max
**26 -> 9**); header/primary overlap screens **66/72 -> 0/72**; screens with
duplicate action labels **70 -> 23** (the remainder are per-row `Read`
actions, never a second primary); primary visible without scrolling
**72/72**; unlabeled dead-end primaries **0**; retry exposed on every failed
configuration read.

## Defects fixed

1. Header and primary action painted over the title, meta and summary at
   every width; the footer also invented an `Apply settings` primary next to
   the real one. Header is now a single scrolling page header and the footer
   owns one primary.
2. The status pill's rounded fill was painted **after** its text, so every
   status looked like an empty white capsule. Fill is now under the text and
   the chip has a short state plus a wrapping reason line.
3. All rows were always visible (up to 26 controls), pushing the primary and
   recovery actions out of view. Three key rows are shown first; the rest is
   behind `Show N more options`, with the full registry rows still in the
   accessibility tree.
4. Save-shaped primaries (`Save Connection`, `Apply Policy`, `Apply Tuning`,
   ...) called a read route. They now commit the staged configuration through
   the one atomic Apply, disabled/`Nothing is staged` when clean.
5. A per-row `Read` beside every editable field duplicated the field and the
   apply action; editable rows now have no probe. Read-only rows keep their
   own probe.
6. `Allow Once` was a review-only action with a decision-shaped label. It is
   `Review Approval` and states that no decision was sent. Agent/skill/rule
   primaries that only read now say `Review ...`; navigation actions open in
   the same browser.
7. `Back` opened a second, nested modal (and Escape did too). The browser now
   installs an in-dialog navigator and a real Back/close handler; the
   duplicate "all 72 screens" combo was removed, leaving the three clusters,
   the local chooser, Settings and Close.
8. 200% text left the fixed header owning the viewport and clipped wrapped
   rows. The header now scrolls with the page, `WrappedLabel` re-wraps on
   width/font changes and revalidates its parents, the scroll body tracks the
   viewport width, and checkboxes meet the 24 px target floor.
9. Destructive Stop Run and Delete Memory now confirm first; typed failures
   name the scope and say nothing was changed.
10. Default values that were prose (`Served catalog default`) were displayed
    as if they were values. Unloaded fields are blank and labelled
    `Not loaded — no value was invented`; Retry reloads the same snapshot.

## Per-screen scores (clarity/density/consistency/efficiency/recovery)

| Screen | Title | Before | After |
|---|---|---|---|
| S01 | Work | 1/1/3/3/4 | 5/5/5/5/5 |
| S02 | Choose a model | 1/1/3/3/4 | 5/5/5/5/5 |
| S03 | Provider connection | 1/1/3/3/4 | 5/5/5/5/5 |
| S04 | Local inference | 1/2/3/3/4 | 5/4/4/5/5 |
| S05 | Model tuning | 1/1/3/3/4 | 5/5/5/5/5 |
| S06 | Choose an agent | 1/2/3/4/4 | 4/5/4/5/5 |
| S07 | Custom agent | 1/3/3/4/4 | 4/5/4/5/5 |
| S08 | Context | 1/2/3/4/4 | 5/5/4/5/5 |
| S09 | Add context | 1/3/3/4/4 | 5/5/4/5/5 |
| S10 | Compact context | 1/2/3/3/4 | 5/5/5/5/5 |
| S11 | Instructions & rules | 1/1/3/3/4 | 5/5/5/5/5 |
| S12 | Memory | 1/3/3/4/4 | 5/4/5/5/5 |
| S13 | Permissions | 1/1/3/3/4 | 5/4/5/5/5 |
| S14 | Approve an action | 1/3/3/3/4 | 5/5/4/5/5 |
| S15 | Tools & MCP | 1/1/3/3/4 | 5/5/5/5/5 |
| S16 | Add MCP server | 1/2/3/3/4 | 5/5/5/5/5 |
| S17 | Skills & commands | 1/3/3/3/4 | 5/5/4/5/5 |
| S18 | Lifecycle hook | 1/1/3/3/4 | 5/4/5/5/5 |
| S19 | Terminal | 1/2/3/3/4 | 5/5/5/5/5 |
| S20 | Browser | 1/3/3/4/4 | 5/5/4/5/5 |
| S21 | Budgets & retries | 1/1/3/3/4 | 5/5/5/5/5 |
| S22 | Live activity | 1/2/3/3/4 | 5/5/5/5/5 |
| S23 | Delegated agents | 1/2/3/3/4 | 5/5/4/5/5 |
| S24 | Review changes | 1/2/3/3/4 | 5/5/5/5/5 |
| S25 | Restore & fork | 1/3/3/3/4 | 5/5/5/5/5 |
| S26 | Conversation history | 1/2/3/3/4 | 5/5/5/5/5 |
| S27 | Usage & outcomes | 1/3/3/3/4 | 5/5/5/5/5 |
| S28 | Efficiency | 1/1/3/3/4 | 5/4/5/5/5 |
| S29 | General settings | 1/1/3/3/4 | 5/5/5/5/5 |
| S30 | Diagnostics | 1/1/3/3/4 | 5/5/5/5/5 |
| S31 | Workspace binding | 1/2/3/3/4 | 5/5/5/5/5 |
| S32 | Task outcome | 1/1/3/3/4 | 5/5/5/5/5 |
| S33 | Task options | 1/3/3/3/4 | 5/5/5/5/5 |
| S34 | First open | 1/3/3/3/4 | 5/4/5/5/5 |
| S35 | Service executable trust | 1/5/3/4/4 | 5/5/5/5/5 |
| S36 | Ready conversation | 1/2/3/3/4 | 5/4/5/5/5 |
| S37 | Attachment admission | 1/5/3/5/4 | 4/5/4/5/5 |
| S38 | Task admission pending | 1/5/3/4/4 | 5/5/5/5/5 |
| S39 | Ambiguous task start | 1/5/3/4/4 | 5/5/5/5/5 |
| S40 | Answer a task question | 1/5/3/4/4 | 5/5/5/5/5 |
| S41 | Inspect overview | 1/3/3/3/4 | 5/5/5/5/5 |
| S42 | Plan and dependencies | 1/5/3/4/4 | 5/5/5/5/5 |
| S43 | Native diff review | 1/5/3/5/4 | 5/5/4/5/5 |
| S44 | Integration conflict | 1/5/3/4/4 | 5/5/5/5/5 |
| S45 | Required verification | 1/4/3/4/4 | 5/5/5/5/5 |
| S46 | Evidence detail | 1/5/3/4/4 | 5/5/5/5/5 |
| S47 | Proof identity mismatch | 1/5/3/4/4 | 5/5/5/5/5 |
| S48 | Finish actions | 1/5/3/4/4 | 5/5/5/5/5 |
| S49 | Cancelled run | 1/5/3/4/4 | 5/5/4/5/5 |
| S50 | Agent detail and steer | 3/5/3/5/4 | 4/5/4/5/5 |
| S51 | Compare approaches | 1/5/3/4/4 | 5/5/4/5/5 |
| S52 | Coordination | 4/5/3/5/4 | 5/5/4/5/5 |
| S53 | Recorded outcome cohort | 1/5/3/4/4 | 5/5/4/5/5 |
| S54 | Usage and quota | 1/5/3/4/4 | 5/5/5/5/5 |
| S55 | Optional control-plane account | 1/5/3/4/4 | 5/5/5/5/5 |
| S56 | Protocol-blocked recovery | 1/5/3/4/4 | 5/5/5/5/5 |
| S57 | Shell isolation setup | 1/5/3/4/4 | 5/5/5/5/5 |
| S58 | Narrow Work panel | 1/5/3/4/4 | 5/5/4/5/5 |
| S59 | Queued messages | 1/5/4/4/4 | 5/5/5/5/5 |
| S60 | Settings revision conflict | 1/5/3/4/4 | 5/5/5/5/5 |
| S61 | Empty conversation history | 4/5/3/5/4 | 5/5/4/5/5 |
| S62 | Unavailable context usage | 1/5/3/5/4 | 5/5/4/5/5 |
| S63 | Compare exact models | 1/4/3/4/4 | 5/5/5/5/5 |
| S64 | Settings search and provenance | 1/5/3/4/4 | 5/5/5/5/5 |
| S65 | Verified outcome | 1/5/3/4/4 | 5/5/5/5/5 |
| S66 | Memory deletion | 1/5/3/5/4 | 5/5/4/5/5 |
| S67 | Hook test result | 1/5/3/4/4 | 5/5/5/5/5 |
| S68 | Future MCP authentication | 1/5/3/4/4 | 5/5/5/5/5 |
| S69 | Instruction precedence | 1/5/3/4/4 | 5/5/4/5/5 |
| S70 | Scoped stop actions | 3/5/3/5/4 | 4/5/4/5/5 |
| S71 | Find in conversation | 4/5/4/5/4 | 5/5/5/5/5 |
| S72 | Switch conversation safely | 3/5/3/5/4 | 4/5/4/5/5 |

## Evidence

- Before/after composites: `target/ux-audit/evidence/before-after-S03.png`,
  `-S14`, `-S29`, `-S34`, `-S58`.
- Per-screen CSV/JSON: `target/ux-audit/evidence/before-scores.csv`,
  `after-scores.csv`, `after-metrics.json`.
- Width/zoom/theme screenshots: `apps/jetbrains/frontend/build/screenshots/`
  (`edition8-s01..s72-{240,320,480,800,hc,200}.png`).
- Host matrix: 1267/1267 checks, including 240/320/480/800 px, 200% text,
  light/dark/high-contrast, keyboard-only focus traversal, dynamic type and
  no horizontal scroll.
- Control exercise: 72/72 screens, 200/200 controls activated, 57 atomic
  applies acknowledged, 3 typed refusals, 0 missing contracts.
- Workflows: 16/16, 80/80 cells.
- Acceptance: 40/40 scenarios.
- Evidence harness: a11y 200/200, optical 3 themes, 81 contrast pairs.
- Performance: `EdtWork` harness (10k tokens @200/s, 500 messages, 1k rows)
  unchanged and passing.

Rev8 control IDs and screen IDs are unchanged. The only intentional merge is
the apply control into the single visible primary on configuration screens;
the legacy apply control remains dispatchable (same ID) for the control
matrix. No fake saved/verified/connected states were introduced. The intentional status-chip/wrapped-label readability change required re-pinning the Edition 8, Edition 5 and parity visual baselines; the permissions baseline was re-rendered and reviewed.
