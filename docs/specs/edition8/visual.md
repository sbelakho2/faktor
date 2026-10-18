# Edition 8 visual contract

Source: `Faktor_IDE_UI_UX_3.pdf` (Revision 8, 17 October 2026), extracted to
`/tmp/faktor-ui3.txt`. This file records the visual tokens, component specs,
host variants, screen/host surface inventory, environment/baseline policy and
screenshot verification rules. Acceptance thresholds are in `acceptance.md`;
CI mapping is in `ci-dogfood.md`.

**Status:** the visual material is design evidence, not product certification.
3.6 says "Plates, measurements and checks in this document are evidence about
the design, not about the shipped product" (p. 196). Real fonts, zoom, HiDPI,
assistive technology and task flows are all "Not run" there.

**Current repo status:** the checked-in visual baselines cover the 32-screen
Edition-5 registry; `docs/specs/edition8/screens.md:901-903` records that the 40
Rev8-added surfaces S33–S72 have no dedicated implementation. Do not treat an
Edition-5 baseline as an Edition-8 screen contract.

## 1. Count ledger

| Item | Count | Source |
|---|---:|---|
| Screens | **72** | Part 2, pp. 22–180 |
| Host contracts | **144** | 72 × VS Code + JetBrains |
| Editable reference vectors | **288** | 72 × 4 host themes (p. 188) |
| Base host-theme renderings | **288** | 72 × 2 hosts × 2 primary themes |
| High-contrast add-on | **144** | 72 × 2 hosts |
| Maximal alternate if every host gets 4 themes | **576** | 72 × 2 hosts × 4 themes |
| Printed visual contract pages | **72** | one per screen |
| Labels in the R8 optical audit | **1,036** | 3.5, p. 195 |
| R7 buttons measured | **288** | 3.5; median −4.64 px |
| Contrast pairs | **80** | 1.11, p. 179; 9 host defaults fail |
| Plate segments checked | **1,497** | 3.6 |
| Strings checked | **5,973** | 3.6 |
| Copy changes listed | **838** | 3.6; VS Code words 5,372 → 3,898 |
| Required button measurements | ≥**30** per host and theme | 3.1 |

Surface-family distribution of the 72 screens (verified from the printed
`VS CODE · ... / JETBRAINS · ...` lines):

| Family | Screens |
|---|---:|
| Sidebar webview / tool window | 38 |
| Editor-tab webview settings / Configurable settings dialog | 16 |
| Native modal message / DialogWrapper | 7 |
| Native Quick Pick / native popup chooser | 3 |
| Native settings editor / Configurable settings dialog | 2 |
| Editor-tab webview / DialogWrapper | 2 |
| Native simple browser / JCEF preview | 1 |
| Sidebar at 320 px / tool window at 320 px | 1 |
| Native panel terminal / console tool window | 1 |
| `vscode.diff` / `DiffManager` | 1 |
| **Total** | **72** |

## 2. Host foundations

The following table is the normative host matrix (p. 9). It is the first thing
to enforce: a webview that sets its own font or a Swing form that hand-orders
buttons is already non-conformant, regardless of pixels.

| Concern | VS Code | JetBrains | Faktor rule |
|---|---|---|---|
| UI font | Platform UI font from `--vscode-font-family`; size from `--vscode-font-size` | Inter on all platforms in the New UI; size from Appearance settings | Never set a font family or size. Plates use DejaVu Sans 13 px (Linux reference) and Inter 13 px. |
| Code font | `--vscode-editor-font-family` and size | Editor colour-scheme font | Monospace only for commands, paths, IDs and output. Plates use DejaVu Sans Mono. |
| Control size | Reference 26 px buttons/inputs in webviews | Reference 28 px; `JBUI`-scaled | Use host components; webview controls never below 24 px. |
| Corners | 2 px controls | 4 px controls; 8–10 px popups/dialogs | Follow the host; no custom radii. |
| Field labels | Stacked: label, one-line description, control | Left label, noun + colon, sentence case | One label component, two layouts. |
| Button labels | Title case, VS Code command style | Title case, imperative verb, ≤5 words; ellipsis when more input follows | Same label text on both hosts; ellipsis only for Locate…, Configure…, Review Restore… and similar. |
| Primary action | Sidebar: full-width at end. Editor tab: left-aligned under form | Tool window: left-aligned row. Dialog: right-aligned, default first (Windows/Linux). Settings: OK, Cancel, Apply | Exactly one primary per surface, named on every spread. |
| Dialog buttons | Modal message API; host orders buttons | DialogWrapper; one default; Help bottom-left | Never hand-order; macOS reverses. |
| Validation | Message box under input (Settings editor); webviews use input-validation tokens | Tooltip above field on focus; inline error above buttons after submit; complex forms keep OK enabled | Format on input; rules on blur or submit; never empty required fields on input. |
| Inline help | Description under setting title | Under control, ≤5 lines / 70 characters; never repeat label | One line default; state a consequence. |
| Persistent settings | Native Settings editor; webviews must not repeat Settings | Configurable pages under Settings › Tools › Faktor | Editor-tab form only for atomic multi-field apply. |
| Simple choices | Quick Pick (single/multi/separators/item buttons) | Popup chooser with search, groups, ad line | Pickers for model, agent, context (S02, S06, S09). |
| Views | One view container; 3–5 views; no sidebar content that should be a command | One tool window with Work, Inspect, History content tabs | Three clusters only; Settings, Usage, Diagnostics open from commands. |
| Theming | All elements from `--vscode-*` | `JBColor`, `UIManager`, `JBUI.scale` | No Faktor colours in reading controls; brand stays on the marketing site. |
| Diff/terminal | `vscode.diff`; integrated terminal in Panel | `DiffManager`; console tool window | Never redraw diffs or terminals in a webview. |
| Focus | 1 px `focusBorder` outline | 2 px accent focus ring | Host focus only; never remove outlines. |

## 3. Tokens

### 3.1 Typography

- Never override the host UI font family or size.
- Code font only for commands, paths, IDs, output.
- Button labels title case; labels/help sentence case.
- One status line per header; no subtitle repeating the title.
- Numbers use locale grouping and a unit; exact values retained for copy/export.
- `120000` renders `120,000`; money `5.00` is exact and copyable.

### 3.2 Geometry and spacing

Base scale printed in 1.8 (p. 14):

| px | Use |
|---:|---|
| 2 | icon to its own label inside a chip |
| 4 | title to meta line; label to control in a stacked field |
| 6 | icon to label; status icon to word |
| 8 | between buttons; row padding |
| 12 | between fields and blocks in a group |
| 14–16 | between groups; section title above rows |
| 24 | between unrelated regions; header to first group in editors |

R7 construction (1.12, p. 19), with R8 status:

| Rule | R7 | R8 status |
|---|---|---|
| Panel insets | 24 px | Superseded: 16 px sidebar content, 24 px editor forms, JetBrains Settings page insets |
| Label column | 156 px + 20 px gutter | Retained for JetBrains label-left rows (128–160 px by page); VS Code stacks labels |
| Controls | 36 px + 16 px row gap | Height superseded: 26 px VS Code / 28 px JetBrains; intrinsic height retained |
| Helper gap | 8 px | Retained; 4–6 px in host metrics; JetBrains errors use platform tooltip |
| Action bar | one footer | Retained where content scrolls; short surfaces place actions after content |

Other fixed metrics:

- Faktor-drawn targets ≥ 24 × 24 CSS px (2.5.8).
- Buttons 26 px (VS Code) / 28 px (JetBrains).
- Text links in rows get 24 px tall hit area; inline sentence links exempt.
- VS Code view-title toolbar icons are 22 px and host-owned (noted, not overridden).
- The 44 × 44 enhanced target is deliberately not adopted (would break host density).
- Focus: 1 px VS Code, 2 px JetBrains; never removed.
- Icons: 16 px box on the label cap centre, 6 px gap; trailing chevrons 12–14 px; icons never set row height.

### 3.3 Breakpoints and reflow

Printed widths across Part 3/4.1/4.6:

| Width | Behaviour |
|---|---|
| 240–319 px | Single column. Name/current phase with Inspect and overflow; top clusters compact. Labels above fields; full-width evidence/finish rows; no fixed key column. Primary and Stop/Deny visible; secondary utilities in a labeled overflow. Diff opens native, never squeezed. |
| 320–479 px | Compact run strip, wrapped metadata, one selected Inspect subview; options scroll inside a bounded area. Primary row wraps; at most two action rows before overflow. |
| 480–719 px | Useful label/value rows, evidence summary + selected detail, single-column transcript; tab/subview text visible. Frequent actions exposed; advanced disclosed. |
| 720+ px | Optional plan/list master-detail with user-adjustable splitter; Work conversation remains focal. More room never adds unrelated dashboards. |
| < 560 px | Labels stack above inputs; multi-choice rows wrap into a deliberate grid. |

- Reflow at 100%, 150%, 200% IDE UI scale plus large user reading fonts. No
  clipped titles, raw hashes as primary labels, or horizontal main-page action
  toolbar.
- Content reflows at 320 CSS px without horizontal scrolling (1.4.10).
- Text resizes to 200% without loss (1.4.4); never shrink text to fit.
- Paths/IDs that cannot wrap elide in the middle and stay copyable in full.
- Code/diff may use explicit contained horizontal scrolling with copy/full view;
  never ellipsize the sole meaning.
- Short height: keep composer reachable; collapse run readout to a summary
  strip; open full options as a scrollable sheet; never stack three equal nested
  scroll panes.
- Arabic RTL and German long text are required fixtures; 50% text expansion is
  the adverse-copy bound.
- 4.1's restored text uses `240/320/480`; 4.6's restored acceptance plan uses
  `240/320/420/720+`; see the ambiguity ledger in `acceptance.md` §9.5.

### 3.4 Colour and contrast

WCAG 2.2 AA: **4.5:1** normal text; **3:1** large text, graphical objects and
component boundaries needed to identify them (1.4.3, 1.4.11). Ratios are
computed from plate tokens. 80 token pairs measured in five themes.

| Pair (need) | Dark Modern | Light Modern | JB Light | JB Dark | High Contrast |
|---|---:|---:|---:|---:|---:|
| Body text (4.5) | 11.06 | 10.55 | 19.76 | 10.55 | 21.00 |
| Body on editor (4.5) | 10.26 | 11.20 | 21.00 | 12.59 | 21.00 |
| Secondary (4.5) | 6.55 | 5.83 | 4.64 | 5.28 | 21.00 |
| Secondary on editor (4.5) | 6.08 | 6.19 | 4.93 | 6.30 | 21.00 |
| Link (4.5) | 7.16 | 5.94 | 6.71 | 4.18 | 7.96 |
| Placeholder (4.5) | 4.51 | 4.54 | 4.93 | 4.26 | 11.42 |
| Primary button label (4.5) | 4.53 | 6.31 | 4.28 | 4.28 | 21.00 |
| Secondary button label (4.5) | 8.10 | 8.89 | 21.00 | 10.55 | 21.00 |
| Primary button vs view (3.0) | 3.92 | 5.94 | 4.03 | 3.23 | 10.55 |
| Focus indicator vs view (3.0) | 3.92 | 5.94 | 4.03 | 3.23 | 8.18 |
| Checkbox/radio outline (3.0) | 4.84 | 4.54 | 3.67 | 4.75 | 10.55 |
| Input border vs editor (3.0) | 1.49 | 1.57 | 1.60 | 2.07 | 10.55 |
| Warning icon (3.0) | 7.69 | 2.93 | 3.17 | 8.49 | 14.97 |
| Error icon (3.0) | 5.30 | 4.46 | 4.17 | 3.87 | 8.55 |
| Success icon (3.0) | 9.74 | 4.07 | 4.15 | 5.61 | 11.52 |
| Info/running icon (3.0) | 5.79 | 3.38 | 4.03 | 4.18 | 6.84 |

Nine host defaults fall below threshold: VS Code Dark input border 1.49:1
(`#3C3C3C` on `#1F1F1F`); VS Code Light input border 1.57:1 (`#CECECE` on
`#FFFFFF`); VS Code Light warning icon 2.93:1 (`#BF8803` on `#F8F8F8`); JB Light
primary button label 4.28:1 (`#FFFFFF` on `#3574F0`); JB Light input border
1.60:1 (`#C9CCD6` on `#FFFFFF`); JB Dark link 4.18:1 (`#548AF7` on `#2B2D30`);
JB Dark placeholder 4.26:1 (`#8C8F96` on `#2B2D30`); JB Dark primary label
4.28:1 (`#FFFFFF` on `#3574F0`); JB Dark input border 2.07:1 (`#4E5157` on
`#1E1F22`).

Decision: keep native components native (host fidelity wins); for Faktor-drawn
elements (status icons, segmented controls, custom checkboxes, focus) require
≥3:1 using the strongest available host token; report the JetBrains
primary-button and link values upstream; verify again in each host's high-
contrast theme. This is a decision, not a silent waiver.

### 3.5 Optical alignment

Cap-height box, not the font line box:

- R7 measured 288 plate buttons: cap centre **−4.64 px** median, range
  −4.41/−4.76; every fixed label sat above midpoint.
- R8 measured **1,036 labels** (buttons, segments, tabs): median **+0.00 px**,
  5th/95th −0.33/+0.17, max |offset| 2.00 px.
- 3.5 per-theme table: JB Dark 274 labels median 0.00; JB Light 274 median
  0.00; VS Code Dark Modern 244 median −0.17; VS Code Light Modern 244 median
  −0.17. Range −0.33/+0.17; max 2.00 (ascender/bowl outliers inspected).
- Webview rule: trim single-line label to cap height + alphabetic baseline and
  centre with layout (`text-box: trim-both cap alphabetic`; fallback
  `line-height: control height`).
- Swing rule: custom paint only when `JButton` cannot; compute cap height with
  `GlyphVector("H")` and centre `(height + cap)/2`; no fixed text offset.
- Form baselines: JetBrains label and field text share one baseline (first
  baseline for wrapped labels); VS Code stacked rows share one left edge.
- Icon box 16 px on the cap centre, 6 px gap; trailing chevron 12–14 px.
- Measurement method: rasterise at 3×; crop interior excluding border and 7 px
  at each end; cap top = first ink row; baseline = median per-column lowest ink
  row; offset = (cap top + baseline)/2 − box centre. Repeat in real hosts on
  at least 30 buttons per host/theme.

## 4. Component specifications

### 4.1 Actions

- **One primary per surface**, named on every spread; it advances the current
  job.
- Label verb + object when ambiguous: Stop Run, Delete Memory, Check Task
  Status, Open AC-03.
- Cancel next to primary; Reset separate (link on JetBrains Settings).
- No red "danger" button in either host. Destructive actions name their
  target, confirm via host dialog, and are never default when a safe action
  exists.
- Disabled primary shows its reason next to it, associated for AT.
- Approvals: Allow Once and Deny equal size; initial focus on the question,
  never on Allow Once.
- Ellipsis only when more input follows.
- Background refresh, optional account and Stop never compete with composer.
- Sidebar/tool-window: primary full-width at end (VS Code) / left row
  (JetBrains). Editor form: left row (VS Code) / OK-Cancel-Apply (JetBrains).
- Composer: Send carries the verb (Run Task/Queue Message); Cmd/Ctrl+Enter.
- Dialog: host-ordered; macOS reverses.
- VS Code Settings saves each valid value; JetBrains Apply is atomic.

### 4.2 Fields

- Label, one-line description/consequence, control; units inside field as
  suffix; values share left edge with their field.
- Help states consequence ("Faktor won't start a call that would go over
  this"), not mechanism ("USD 5.00 = 5,000,000 micro-USD").
- Unset ≠ zero: empty shows inherited value as placeholder; `0 means no limit`
  only where backend uses zero that way.
- Locked values show who set them ("Set by your organization") + lock icon.
- Validate characters on input; format/range on blur/submit; remote checks
  only via explicit Test.
- Errors describe problem + fix, keep user text; editing hides error.
- VS Code: message box under input. JetBrains: tooltip above field on focus;
  inline error above buttons after submit; focus first invalid field.
- Whole-form apply: VS Code native Settings saves only valid values and reports
  conflicts; JetBrains atomic inline error; nothing saved on invalid.

### 4.3 Choices

| Control | Use | Spec |
|---|---|---|
| Segmented | 2–4 exclusive short options ≤12 chars | Reasoning effort, Finish, Restore scope |
| Radio group | Exclusive options needing one line each | Shell isolation, Stop scope, hook failure |
| Checkbox | Independent on/off | Efficiency switches, instruction sources |
| Select | ≥5 options or runtime-loaded | API, adapter, keep loaded |
| Picker | Searchable with capabilities/groups | Model, agent, context |

- Segmented and radio use native checked states; checkbox rows wrap with label
  and help kept together; pickers are host-native (Quick Pick / popup chooser).

### 4.4 Status, attention, recovery

| Group | Words | Recovery |
|---|---|---|
| Done | Done, Passed, Verified, Ready, Kept | None |
| Running | Running, Starting…, Checking AC-03 | Stop (scoped) |
| Waiting | Waiting for your approval, Needs input, Out of date, Conflict | Open, Review, Verify current candidate |
| Failed | Failed, Action needed, Blocked by an error | Retry, Sign In…, Configure Remote… |
| Pending | Pending, Needs evidence, Incomplete | Open criterion/step |
| Blocked | Blocked, Denied by preset, Can't restore | Reason shown; fix where it exists |
| Quiet | Not started, Queued, Unavailable, Stopped, Cancelled | Quiet; no colour emphasis |
| Locked | Locked (organization) | Who set it |

- Status is always icon + word; colour never the only signal.
- One status line per header; do not repeat state in badge, subtitle and row.
- Banners only for screen-wide conditions; one at a time.
- Decision-critical attention (approval, question) pinned above transcript
  with facts visible; every problem offers its own recovery.
- Pending, stale, unavailable, failed and effect-unknown never render as
  success.

### 4.5 Lists, rows, disclosure, tabs

- Shallow master/detail; one selected detail; Back returns to exact row.
- Row: 16 px icon, title, at most one meta line, at most one trailing status or
  action (both stack on separate baselines); 8 px vertical padding.
- Rows bind to entity IDs; refresh keeps selection and scroll anchor.
- Disclosure: chevron + title + one-line summary; expanded state exposed
  (`aria-expanded` / `AccessibleState.EXPANDED`).
- Tabs: VS Code panel-style uppercase tabs inside webview; JetBrains
  tool-window content tabs; arrow-key tab semantics.
- Read-only facts are text, never disabled inputs.

### 4.6 Dialogs, pickers, diff, terminal

- Modal: native message (VS Code) / DialogWrapper (JetBrains); target and
  effect named; one default; Help bottom-left on JetBrains.
- Quick Pick / popup chooser: host-native accessibility; searchable; Enter
  selects; multi-select shows OK in VS Code.
- Diff: `vscode.diff` / `DiffManager` only; immutable base/candidate pane
  titles; read-only; Faktor adds one "Open Related Criterion" action; never
  squeeze side-by-side into a tool window.
- Terminal: VS Code integrated terminal in Panel; JetBrains console tool
  window; bounded buffer; never redraw in webview.
- Browser: native simple browser (VS Code) / JCEF preview (JetBrains); external
  content is not a Faktor webview.

### 4.7 Copy and progressive disclosure

- Title Case verbs on buttons; sentence case labels/help; name objects exactly;
  spell out acronyms; avoid "please", "successfully", "!".
- Four tiers: Tier 1 always visible; Tier 2 tooltip/disclosure; Tier 3
  Inspector views; Tier 4 documentation. Counts: 711 rewritten in place; 61
  tooltips/disclosures; 7 inspector views; 59 spec/doc.
- Approval command, target, network and effect are never lazily disclosed.
- Nothing is removed: every relocation is recorded per screen (838 changes).

## 5. Host variants

### 5.1 VS Code

- Stable release current at test time; Windows 11, macOS, Ubuntu.
- Themes: Dark Modern, Light Modern, High Contrast Dark, High Contrast Light.
  High contrast: borders carry component boundaries; focus is 1 px orange
  outline.
- Layouts: sidebar 240/320/480 px, secondary sidebar, panel.
- Webviews inherit `--vscode-*`; native Settings editor for persistent
  preferences; editor-tab form only for atomic multi-field apply.
- 26 px controls, 2 px corners, 1 px focus, 16 px sidebar inset, 24 px editor
  inset.
- Quick Pick for simple choices; native modal message; `vscode.diff`;
  integrated terminal in Panel.
- Host orders dialog buttons; macOS reversal.

### 5.2 JetBrains

- IntelliJ IDEA plus one other IDE on the same platform version; New UI.
- Themes: Light, Dark, High Contrast (High Contrast must be checked in the
  IDE, p. 193).
- Layouts: tool window docked right/bottom, floating, 320 px.
- Configurable pages under Settings › Tools › Faktor; DialogWrapper for modal;
  popup chooser; `DiffManager`; console tool window; JCEF preview.
- Inter 13 px, 28 px controls, 4 px controls, 8–10 px popups/dialogs, 2 px
  focus, `JBUI.scale` for all metrics.
- Label-left rows (128–160 px label column); label and field share baseline;
  Help bottom-left; OK/Cancel/Apply, Reset link top-right.
- Tool window tabs for Work/Inspect/History; one tool window; no second nested
  detail pane.

### 5.3 Native surface rules

- Persistent settings never live in a webview on VS Code; JetBrains uses
  Configurable pages.
- Atomic multi-field forms use an editor-tab form only where required.
- Simple choices use native pickers; do not recreate a picker in a webview.
- Diff and terminal never move into a webview.
- Focus/tokens come from the host; no Faktor-specific reading colours.
- Brand mark stays on marketing surfaces; the fixed angular F identifies the
  product; no circular/koru ornament; native host reading/editor fonts and
  focus/theme tokens take precedence over Barlow/Ochre.

## 6. Screen/host surface inventory (72)

This is the complete printed matrix. `Controls` are the IDs printed on the
screen's design page; every screen also has a full contract page. The
inventory is the coverage denominator: a host contract exists only when the
screen renders in that host surface.

| ID | Screen | Group | VS Code surface | JetBrains surface | Primary action | Controls | p. |
|---|---|---|---|---|---|---|---:|
| S34 | First open | Setup and start | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Start Faktor | `O-04 O-05 P-01 Q-11` | 22 |
| S35 | Service executable trust | Setup and start | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Confirm Path | `O-04 O-05` | 25 |
| S57 | Shell isolation setup | Setup and start | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Keep Fail-Closed · JetBrains: OK / Apply | `Q-10 Q-11` | 27 |
| S36 | Ready conversation | Setup and start | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Run Task (composer) | `A-01 M-02 Q-01 V-07 V-10 B-01 B-02` | 29 |
| S33 | Task options | Setup and start | EDITOR TAB · WEBVIEW | DIALOGWRAPPER | Use Options | `V-07 V-10 B-01 B-02` | 31 |
| S37 | Attachment admission | Setup and start | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Use Attachments | `C-03 C-05 C-06` | 33 |
| S38 | Task admission pending | Setup and start | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Queue Message (composer) | `D-05 V-07` | 35 |
| S39 | Ambiguous task start | Setup and start | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Check Task Status | `B-12 D-05` | 37 |
| S01 | Work | Work | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Queue Message (composer) | `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10` | 40 |
| S14 | Approve an action | Work | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Allow Once | `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12` | 42 |
| S40 | Answer a task question | Work | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Send Answer | `D-07 V-07` | 44 |
| S22 | Live activity | Work | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Approval | `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04` | 46 |
| S59 | Queued messages | Work | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Save Edit | `D-05` | 49 |
| S70 | Scoped stop actions | Work | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Stop Run | `A-10 X-05 D-07` | 51 |
| S41 | Inspect overview | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Missing Evidence | `V-07 V-08 V-09 V-10` | 55 |
| S42 | Plan and dependencies | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open AC-03 | `V-07 V-08` | 57 |
| S24 | Review changes | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Diff | `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11` | 59 |
| S43 | Native diff review | Inspect and proof | DIFF EDITOR · VSCODE.DIFF | DIFF VIEWER · DIFFMANAGER | Open Related Criterion | `V-01 V-03` | 61 |
| S45 | Required verification | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open AC-03 Details | `V-07 V-08 V-09` | 63 |
| S46 | Evidence detail | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Full Output | `V-07 V-08` | 65 |
| S47 | Proof identity mismatch | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Verify Current Candidate | `V-09 V-12` | 67 |
| S44 | Integration conflict | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Conflict | `V-01 V-12` | 69 |
| S48 | Finish actions | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Configure Remote… | `V-10 V-11` | 71 |
| S32 | Task outcome | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Required Verification | `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04` | 73 |
| S65 | Verified outcome | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Integrated Change | `V-09 V-10` | 75 |
| S25 | Restore & fork | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Restore… | `V-04 V-05 V-06 V-12 D-06 Q-09` | 77 |
| S49 | Cancelled run | Inspect and proof | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Kept Work | `D-07 V-04 B-04` | 79 |
| S23 | Delegated agents | Agents | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Send Steer | `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06` | 82 |
| S50 | Agent detail and steer | Agents | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Send Steer | `A-09 A-10 A-11 A-12` | 84 |
| S51 | Compare approaches | Agents | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review Approach B | `M-14 A-12 V-07` | 86 |
| S52 | Coordination | Agents | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Post Note | `A-12 D-07` | 88 |
| S26 | Conversation history | History | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Conversation | `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09` | 91 |
| S61 | Empty conversation history | History | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Start a Task | `D-01 D-02` | 93 |
| S71 | Find in conversation | History | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Go to Message | `D-03` | 96 |
| S72 | Switch conversation safely | History | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Switch | `D-01 D-15 X-13` | 98 |
| S53 | Recorded outcome cohort | Usage | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Runs | `O-06 K-12 M-13` | 101 |
| S54 | Usage and quota | Usage | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | None (read-only) | `O-06 O-07` | 103 |
| S27 | Usage & outcomes | Usage | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Inspect Runs | `B-04 M-13 K-12 O-06 V-09 D-07` | 105 |
| S02 | Choose a model | Models and providers | QUICK PICK · NATIVE | POPUP CHOOSER · NATIVE | Enter selects | `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14` | 108 |
| S03 | Provider connection | Models and providers | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save Connection · JetBrains: OK / Apply | `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14` | 110 |
| S04 | Local inference | Models and providers | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save · JetBrains: OK / Apply | `P-07 P-08 P-09 P-13 P-15 C-10 M-09` | 112 |
| S05 | Model tuning | Models and providers | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Apply Tuning · JetBrains: OK / Apply | `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14` | 114 |
| S63 | Compare exact models | Models and providers | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Start Comparison | `M-14 B-01 B-02` | 116 |
| S06 | Choose an agent | Models and providers | QUICK PICK · NATIVE | POPUP CHOOSER · NATIVE | Enter selects | `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12` | 118 |
| S07 | Custom agent | Models and providers | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save Agent · JetBrains: OK / Apply | `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05` | 120 |
| S08 | Context | Context | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Sources | `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14` | 123 |
| S62 | Unavailable context usage | Context | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Open Sources | `C-01 C-02` | 125 |
| S09 | Add context | Context | QUICK PICK · NATIVE | POPUP CHOOSER · NATIVE | Enter selects · Add 3 Items | `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11` | 127 |
| S10 | Compact context | Context | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Compact | `K-01 K-02 K-03 K-04 C-12 I-11 V-07` | 129 |
| S11 | Instructions & rules | Instructions and memory | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Add Rule… · JetBrains: OK / Apply | `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11` | 133 |
| S69 | Instruction precedence | Instructions and memory | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Open Project Instructions · JetBrains: OK / Apply | `I-04 I-08 Q-12` | 135 |
| S12 | Memory | Instructions and memory | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save Memory · JetBrains: OK / Apply | `K-05 K-10 K-11 O-09 I-11` | 137 |
| S66 | Memory deletion | Instructions and memory | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Delete | `K-10 K-11` | 139 |
| S13 | Permissions | Permissions and tools | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Apply Policy · JetBrains: OK / Apply | `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14` | 142 |
| S15 | Tools & MCP | Permissions and tools | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Add Server… · JetBrains: OK / Apply | `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15` | 144 |
| S16 | Add MCP server | Permissions and tools | EDITOR TAB · WEBVIEW | DIALOGWRAPPER | Save and Start | `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15` | 146 |
| S68 | Future MCP authentication | Permissions and tools | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Open Sign-In Page | `T-04 T-06` | 148 |
| S17 | Skills & commands | Permissions and tools | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Install Skill… · JetBrains: OK / Apply | `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06` | 150 |
| S18 | Lifecycle hook | Permissions and tools | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save Hook · JetBrains: OK / Apply | `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14` | 152 |
| S67 | Hook test result | Permissions and tools | MODAL MESSAGE · NATIVE | DIALOGWRAPPER | Back to Hook | `S-12 S-13` | 154 |
| S19 | Terminal | Permissions and tools | PANEL TERMINAL · NATIVE | TOOL WINDOW · CONSOLE | Open Full Output | `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10` | 156 |
| S20 | Browser | Permissions and tools | SIMPLE BROWSER · NATIVE | JCEF PREVIEW WINDOW | Capture Screenshot | `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03` | 158 |
| S21 | Budgets & retries | Settings and diagnostics | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Apply Defaults · JetBrains: OK / Apply | `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12` | 161 |
| S28 | Efficiency | Settings and diagnostics | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Apply · JetBrains: OK / Apply | `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11` | 163 |
| S29 | General settings | Settings and diagnostics | SETTINGS EDITOR · NATIVE | SETTINGS DIALOG · CONFIGURABLE | VS Code: Settings saves valid values · JetBrains: OK / Apply | `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14` | 165 |
| S64 | Settings search and provenance | Settings and diagnostics | SETTINGS EDITOR · NATIVE | SETTINGS DIALOG · CONFIGURABLE | VS Code: Settings saves valid values · JetBrains: OK / Apply | `O-01 D-11` | 167 |
| S60 | Settings revision conflict | Settings and diagnostics | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Save Resolved Settings · JetBrains: OK / Apply | `O-01 M-12` | 169 |
| S55 | Optional control-plane account | Settings and diagnostics | EDITOR TAB · WEBVIEW SETTINGS | SETTINGS DIALOG · CONFIGURABLE | Connect Account · JetBrains: OK / Apply | `O-07 P-04` | 171 |
| S30 | Diagnostics | Settings and diagnostics | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Preview Export… | `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13` | 173 |
| S56 | Protocol-blocked recovery | Settings and diagnostics | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Restart Service… | `O-04 D-13` | 175 |
| S31 | Workspace binding | Settings and diagnostics | SIDEBAR VIEW · WEBVIEW | TOOL WINDOW · SWING / KOTLIN UI DSL | Review New Setup | `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11` | 177 |
| S58 | Narrow Work panel | Settings and diagnostics | SIDEBAR VIEW AT 320 PX | TOOL WINDOW AT 320 PX | Queue Message (composer) | `D-08 D-09 D-14` | 179 |

The table's order is the printed design order (Setup → Work → Inspect → Agents → History → Usage → Models → Context → Instructions → Permissions → Settings), not numeric ID order. `S05` and `S04` both appear on the printed spreads for pp. 112–114; `S17`/`S18`/`S67` share the permissions section pages 150–154 but have separate contract pages. A screen is in scope only with its own contract page and both host surfaces.


## 7. Screen-specific visual contracts

The table above is the surface map; the following points are the visual rules
that differ from the shared component set. All are printed in the named
contract.

| Screen | Visual contract |
|---|---|
| S01 Work | Compact tool window: intent, model, effort, authority and context beside draft; one header status; approval pinned above transcript; composer with exact model/effort controls; no second nested detail pane. |
| S14 Approve an action | Approval dock keeps requester/owner, operation, target, reason and effect boundary visible without losing transcript; Allow Once and Deny equal size; focus starts on question; Details disclosure; one pinned attention. |
| S19 Terminal | Native host terminal/console only; bounded output; process owner/generation shown; Stop Process scoped and separate from task stop/session abort; no webview redraw. |
| S20 Browser preview | Native simple browser/JCEF; external content clearly outside Faktor webview; Open Externally; no Faktor theme tokens imposed on page. |
| S22 Live activity | Timeline marks active/waiting/queued/failed/uncertain; critical attention visible when details collapsed; no fabricated percentages or optimistic completion. |
| S24 Review changes | Master/detail; native diff opens in editor; base/candidate identity, criteria and pending verification adjacent; Add Review Task. |
| S25 Restore & fork | List of recorded checkpoints; restore/fork preview names file preconditions and uncovered shell/network/SCM effects; missing blobs disable redo/diff. |
| S29 General settings | VS Code native Settings editor filtered to Faktor; JetBrains Tools › Faktor page. Invalid `five` shows native/per-setting or tooltip+inline error; machine path help; Reset link + OK/Cancel/Apply. |
| S30 Diagnostics | Readiness/version/runtime identity; every problem row carries its fix; healthy rows quiet; redacted export preview; no credentials/raw env; bounded error excerpts; source content off by default. |
| S31 Workspace binding | Editor/execution/model/vault/candidate locations together; remote boundaries explicit; paths wrap; never infer local from a loopback URL. |
| S35 Service executable trust | Native modal: exact path, source, checksum "Not recorded", separate publisher trust; "Confirm only if you trust this exact file"; no signature claim from checksum. |
| S43 Native diff review | Host diff viewer; immutable base/candidate pane titles; read-only; one Faktor action; 1 of 6 changes; hunk navigation native. |
| S56 Protocol-blocked recovery | Last coherent snapshot/cursor; "Live updates paused"; "Nothing was skipped"; last update time; draft kept; run view out of date; technical details; Restart Service…/Copy Diagnostics. |
| S57 Shell isolation setup | Supported modes with platform-established filesystem/network guarantees; fail-closed default; "Network-capable — weaker isolation" not "complete isolation"; setup failure blocks affected actions. |
| S58 Narrow Work panel | At 320 px header, one detail row and composer stack; Stop stays in header; Enter newline; IME composition; Escape closes detail, never stops run. |
| S61 Empty history | True empty vs no search matches vs failed reads distinct; "No conversations yet"; Start a Task; unsent draft separately retained with Open Draft. |
| S62 Unavailable context | Null usage renders "Unavailable", no percentage/bar; draft estimate separate "About 17,000 tokens · estimate"; model capacity may be known. |
| S64 Settings search/provenance | Native Settings search; scope/provenance per value; lock/source; Remove Override deliberate; no duplicate webview settings. |
| S68 Future MCP authentication | Reviewed stdio only unless remote/OAuth is implemented; transport, auth and health distinct; cancelled auth not Connected. |
| S72 Switch conversation safely | Native modal for session switch; old handles/generation fenced; drafts preserved; no credentials copied; explicit switch. |

## 8. Environment and baseline policy

The spec's required matrix (3.1) is deliberately "current at test time":

| Axis | Required |
|---|---|
| VS Code | Stable release current at test time; Windows 11, macOS, Ubuntu |
| JetBrains | IntelliJ IDEA + one other IDE on the same platform version; New UI |
| Themes | VS Code Dark Modern/Light Modern/High Contrast Dark/High Contrast Light; JetBrains Light/Dark/High Contrast |
| Layouts | VS Code sidebar 240/320/480 px, secondary sidebar, panel; JetBrains tool window right/bottom, floating, 320 px |
| Zoom | 100/150/200% host zoom; OS text scaling where supported; 200% text at 320 px |
| Locale | English; German long text; Arabic RTL |
| Adverse | 50% text expansion; long paths/model IDs/multi-line errors |
| Assistive | NVDA (Windows), VoiceOver (macOS), Orca (Ubuntu, VS Code) |
| Optical | ≥30 buttons per host/theme; cap-centre method |
| Contrast | Rendered UI in each theme; resolve 1.11 host-token findings |
| People | ≥5 weekly agent developers per host + ≥2 screen-reader users |

Baseline rules:

1. **No version inheritance.** The PDF says "current at test time"; it does
   not pin VS Code, JetBrains, OS or fonts. A record must name the exact host
   build, OS build, locale, theme, zoom, width, font fingerprint and fixture.
2. **No platform inheritance.** A pass on Linux is not a pass on macOS or
   Windows. The repo's existing platform checker encodes this for JetBrains
   (`scripts/check-visual-platforms.mjs:16-19`); the same rule should govern
   VS Code. (Repo rule, not PDF text.)
3. **Reference vs shipped.** The plates are Chromium with substitute fonts;
   real hosts use Segoe UI/SF Pro/Inter and their own rasterisers. The 3.5
   measurement must be repeated in the real hosts.
4. **High contrast is extra.** The build renders four host themes; high
   contrast is required in the IDE in addition (p. 193). Do not treat HC as a
   substitute for Dark/Light.
5. **320 px stands for 200% text in a webview** (CSS zoom), but JetBrains
   Swing must still be checked at 200% IDE zoom in the host (p. 194).
6. **Native surfaces are not screenshots of webviews.** Settings editor,
   Quick Pick, modal, diff, terminal and JCEF/browser surfaces must be captured
   from the host, not recreated as HTML.

## 9. Screenshot and verification rules

1. **A screenshot is evidence only with a fixture record.** Record screen ID,
   host, host build, OS, theme, width, zoom, locale, state (first-read /
   empty / ready / stale / error / pending / acknowledged), and the exact
   artifact hash. A bare image is not a contract pass.
2. **Every screen in both hosts**: "Review every screen with
   empty/large/invalid/stale data; 240/320/480 px tool windows; 200% text; long
   names; keyboard/IME; both host themes/high contrast; and running stream.
   Capture reference snapshots at fixed fixtures; reject overlap, hidden
   actions, off-grid helper text or mismatched state before merge." (4.3, p.
   237).
3. **Every screen in four host themes**: 72 × 4 = 288 reference vectors; the
   build must render all four themes. High contrast and native font/zoom
   integration remain required (p. 188).
4. **No clipped content outside intended scroll regions.** 3.6 reports this
   as done in the reference renderer; real-host overflow/collision checks are
   still required.
5. **Optical audit**: rasterise at 3×, crop interior (exclude border and 7 px
   per end), measure cap-top/baseline, compare to box midpoint; ≥30 buttons per
   host/theme. Report median, mean, 5th/95th, max |offset|. A fixed text offset
   is forbidden.
6. **Contrast**: compute WCAG 2.2 AA from rendered tokens; normal text 4.5:1,
   large text/graphics/boundaries 3:1. Report the nine host-default failures
   and the upstream decision; Faktor-drawn elements must use ≥3:1.
7. **Focus visibility**: host indicator present at every stop; sticky footers
   and pinned cards never cover the focused control (2.4.11); no focus trap;
   opening a detail moves focus to its heading/first useful control; closing
   returns to exact trigger; disappearing item goes to nearest stable sibling
   or section heading.
8. **Announcements**: new requests/final outcomes once; approvals once without
   stealing focus; streaming prose bounded polite updates; no token-by-token
   announcement; colour never the only state.
9. **Reduced motion**: static state cues remain; screen-reader mode avoids
   ornamental animation.
10. **No fake states in visual evidence**: do not paint Saved/Connected/
    Verified/Complete/Allowed/Stopped/Idle/0%/$0/Isolated/Remembered/Deleted
    without the backing state; do not describe generated prototype scenes as
    runtime screenshots or certification (4.6 p. 332).
11. **Do not use the PDF plates as shipped screenshots.** 3.6 and 4.6 both
    state plates/measurements are design evidence, not product evidence.

## 10. Ambiguities

1. **288 vs 576 renderings.** The document says 288 editable vectors (72 × 4
   themes) and also says every surface is in both host themes. The PDF never
   prints a host-render total. Conservative reading used here: 288 reference
   vectors, 288 base host-theme renderings (2 primary themes per host), plus
   144 high-contrast = 432; 576 is the maximal alternate if each host gets all
   four themes. Do not silently drop the extra themes.
2. **Four themes vs five names.** Build says four host themes; contrast table
   names five columns (Dark Modern, Light Modern, JB Light, JB Dark, High
   Contrast). High contrast is separately mandatory in the IDE. Use the five
   named themes plus IB high contrast; do not drop any.
3. **240/320/480 vs 240/320/420/720+.** Part 3/4.1 use 480; the restored
   acceptance plan (4.6) uses 420. Run the union until resolved.
4. **"At least 30 buttons per host and theme"** does not enumerate themes or
   whether high contrast is separate. Conservative: all themes including HC.
5. **"Current at test time"** is not a pinned version; the harness must record
   exact builds. The PDF does not specify a font fingerprint format.
6. **JetBrains "one other IDE"** is unspecified; choose a real second IDE on
   the same platform version and record it.
7. **RTL scope.** Arabic is required, but the PDF only states reflow/no
   clipping; it does not print a separate RTL screenshot per screen.
8. **Native surface screenshots.** The PDF calls the diff/settings/terminal
   native but does not define a screenshot schema; the harness must define one
   without reducing the contract to webview-only images.
9. **Contrast decision is open.** The document says "decisions open" for the
   nine host defaults; do not silently waiver them. Record the upstream report
   and the Faktor-drawn mitigation.
