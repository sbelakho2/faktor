# Edition 8 screens and native hosts (S01–S72)

Source: `Faktor_IDE_UI_UX_3.pdf` Revision 8 (17 Oct 2026), screen interaction contracts and reference plates pp.20–180; product IA pp.197–211; shared components pp.8–19; visual identity v3 pp.359–382. This file is the product/interaction inventory; acceptance/CI/visual evidence is owned by the sibling artifact set.

## Counts and structure

- **72 distinct screens/native hosts** in 11 groups. Every screen has a design plate and a behaviour contract; every control ID on a plate is parsed from its `CONTROLS` footer.
- **Setup and start:** 8 (S34, S35, S57, S36, S33, S37, S38, S39).
- **Work:** 6 (S01, S14, S40, S22, S59, S70).
- **Inspect and proof:** 13 (S41, S42, S24, S43, S45, S46, S47, S44, S48, S32, S65, S25, S49).
- **Agents:** 4 (S23, S50, S51, S52).
- **History:** 4 (S26, S61, S71, S72).
- **Usage:** 3 (S53, S54, S27).
- **Models and providers:** 7 (S02, S03, S04, S05, S63, S06, S07).
- **Context:** 4 (S08, S62, S09, S10).
- **Instructions and memory:** 4 (S11, S69, S12, S66).
- **Permissions and tools:** 9 (S13, S15, S16, S68, S17, S18, S67, S19, S20).
- **Settings and diagnostics:** 10 (S21, S28, S29, S64, S60, S55, S30, S56, S31, S58).

Control-to-screen edges: 200 controls, 1–13 screens per control; the union of screen footers is exactly the 200 control IDs. Screen entries below preserve the PDF spread order, not numeric order.

## Global visual and state contract

- **Spacing:** 4-step scale; within-group 4–8 px, between groups 14–16 px, section gaps 24 px. Settings use 24 px insets, 156 px label column, 20 px column gap, 36 px controls, 8 px help gaps, 16 px row gaps (pp.14, 197–211).
- **Targets:** Faktor controls ≥24 px tall; buttons 26/28 px; primary composer action 36 px where host metrics permit (pp.17, 197–211).
- **Responsive:** 240–319 px labelled vertical stack with Send/Stop/attention visible; 320–479 px two compact wrapped rows; ≥480 px full configuration summary. S58 is the 320 px reference (pp.194, 179).
- **Typography:** inherit host UI/editor font, size and zoom; technical metadata uses host monospace. Brand Barlow/Barlow Semi Condensed/IBM Plex Mono are for documentation and identity, not native reading controls (pp.359–374).
- **Color:** host semantic tokens; Faktor identity Ink `#08110C`, Forest `#143D2B`, Paper `#F3F5EF`, Sage `#92AA99`, Ochre `#D9BA75`, Ochre Deep `#72541E`; Ochre/Paper is decorative only (1.7:1); never rely on color alone for state (pp.372–373).
- **State vocabulary:** one status line per header (icon + word); running, waiting, queued, failed, cancelled, uncertain, pending and verified are distinct. Empty names the next action; loading is bounded/cancellable; stale keeps the last coherent snapshot with age; error names the typed cause, scope and safe recovery; disabled names the missing prerequisite. Optimistic paint is limited to expansion/filter/favorite/draft; Saved/Connected/Allowed/Cancelled/Restored/Verified/PR-created require their own acknowledgement (pp.13, 222).
- **Host variants:** VS Code uses the activity view (Work/Inspect/History), editor-sized settings tabs, Quick Pick, native diff/terminal and typed webview bridge. JetBrains uses a dockable tool window, `Settings › Tools › Faktor` Configurable, popup choosers, DialogWrapper, native diff and console (pp.9, 20–180). Both hosts share semantic fixtures; neither webview nor Swing may decide policy/ledger outcomes.

## Screen contracts

### S34 — First open

**Group / job.** Setup and start. Open a project, review prerequisites and start the service explicitly while preserving the draft.
**Primary action.** Start Faktor
**Controls (4).** `O-04 O-05 P-01 Q-11`
**Layout regions.** Work header and tab row (Work / Inspect / History) · Setup panel status: “Set up Faktor / Faktor isn’t running” · Prerequisite checklist: service executable, model, shell isolation · Draft-retained notice · Sticky action row: Start Faktor, Setup Guide
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Service not running; prerequisites unselected/not connected/fail-closed; draft retained.
**Navigation.** Entered from Work when the service/model/setup is absent; Back returns to Work with the draft intact; Setup Guide opens the setup documentation.
**Traceability.** Design p.22; contract p.23; controls `O-04 O-05 P-01 Q-11`.

### S35 — Service executable trust

**Group / job.** Setup and start. Inspect the exact machine executable and its separate checksum and publisher trust basis before any start.
**Primary action.** Confirm Path
**Controls (2).** `O-04 O-05`
**Layout regions.** Modal title and executable path · Bounded executable source preview · Source checksum row and publisher/signature basis · Trust confirmation checkbox (“I reviewed this executable path”) · Confirm Path / Show Details / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; executable preview; trust checkbox.
**States.** Checksum not recorded; signature not verified; trust is per exe path and machine; cancel keeps the previous trust state.
**Navigation.** Opened from S34 “Locate…” or the runtime path setting; Confirm Path returns to S34; Cancel keeps the previous trust state.
**Traceability.** Design p.25; contract p.26; controls `O-04 O-05`.

### S57 — Shell isolation setup

**Group / job.** Setup and start. Choose supported shell isolation explicitly and show platform-established filesystem and network guarantees.
**Primary action.** Keep Fail-Closed · JetBrains: OK / Apply
**Controls (2).** `Q-10 Q-11`
**Layout regions.** Machine-setting header and reset · Isolation choice: fail-closed (recommended) / network-capable · Reported filesystem and network capabilities · Open Platform Shell Setup action · Settings footer: OK / Cancel / Apply
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; isolation radio; platform capability rows.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from S34 “Review…” or Settings > Faktor > Shell Isolation; OK/Apply returns to the invoker; Platform Shell Setup opens host settings.
**Traceability.** Design p.27; contract p.28; controls `Q-10 Q-11`.

### S36 — Ready conversation

**Group / job.** Setup and start. Write one task, choose an exact model and intent, and review the captured authority, criteria, finish and run caps.
**Primary action.** Run Task (composer)
**Controls (7).** `A-01 M-02 Q-01 V-07 V-10 B-01 B-02`
**Layout regions.** Work header · Goal composer with placeholder · Task-options summary (effects, finish, cap) · Context and model/effort row · Run Task primary action; Ctrl/Cmd+Enter
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar composer; one primary Run Task; one status line.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Work destination; Run Task admits the captured task; Task options and attachments open as sheets without losing the draft.
**Traceability.** Design p.29; contract p.30; controls `A-01 M-02 Q-01 V-07 V-10 B-01 B-02`.

### S33 — Task options

**Group / job.** Setup and start. Edit the goal, required criteria, completion contract, finish level and exact run caps in one draft sheet. The active task keeps its
**Primary action.** Use Options
**Controls (4).** `V-07 V-10 B-01 B-02`
**Layout regions.** Goal field · Acceptance-criteria rows and Add Criterion · Constraints, risks and claims summary · Finish ladder Local / Commit / Push / PR · Token cap and money cap with limit behavior · Use Options / Cancel
**Host variants.** VS Code: Dark Modern editor tab · webview. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Editor-tab sheet/dialog; criteria list; finish ladder; caps.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from the Work composer “Task options”; Use Options applies to the draft; Cancel keeps the prior captured options.
**Traceability.** Design p.31; contract p.32; controls `V-07 V-10 B-01 B-02`.

### S37 — Attachment admission

**Group / job.** Setup and start. Resolve file, image and upload admission per source; failed or unsupported sources remain visible in the draft.
**Primary action.** Use Attachments
**Controls (3).** `C-03 C-05 C-06`
**Layout regions.** Attachment list with per-source status · Selected source details (type, size, range, version) · Unsupported/failed rows with Remove/Retry · Add Files action · Use Attachments
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; attachment rows; failed/unsupported rows stay visible.
**States.** 2 need attention; unsupported image for the chosen model; interrupted upload; retry/remove; failed sources stay visible.
**Navigation.** Opened from the Work composer attachment control; Use Attachments returns to the draft; failed sources remain until removed/retried.
**Traceability.** Design p.33; contract p.34; controls `C-03 C-05 C-06`.

### S38 — Task admission pending

**Group / job.** Setup and start. Show one captured logical submission while the admission receipt is pending; suppress duplicate starts.
**Primary action.** Queue Message (composer)
**Controls (2).** `D-05 V-07`
**Layout regions.** Sent user message · “Starting…” admission status · Running configuration summary (Code, effects, finish) · Composer with next-message note · Context/model/usage row
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; starting state; composer stays available.
**States.** Starting…; duplicate starts suppressed; next message does not change the submitted start.
**Navigation.** Work destination immediately after Run Task; starting state resolves to admitted or S39; the composer stays available for the next queued message.
**Traceability.** Design p.35; contract p.36; controls `D-05 V-07`.

### S39 — Ambiguous task start

**Group / job.** Setup and start. Reconcile an ambiguous admission against the original request identity and body before any permitted retry.
**Primary action.** Check Task Status
**Controls (2).** `B-12 D-05`
**Layout regions.** Start-not-confirmed banner · Connection-dropped explanation · Kept draft summary: task/criteria, attachments/options · Retry-safety note (same request cannot start twice) · Check Task Status / Keep Draft
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; banner + kept-draft summary.
**States.** Start not confirmed; connection dropped after sending; task/criteria and attachments/options kept; retry sends the same request identity.
**Navigation.** Opened from S38 when admission is ambiguous; Check Task Status reconciles the original request; Keep Draft returns to Work.
**Traceability.** Design p.37; contract p.38; controls `B-12 D-05`.

### S01 — Work

**Group / job.** Work. The compact tool window exposes intent, model, effort, authority and context beside the draft. Running work keeps the active
**Primary action.** Queue Message (composer)
**Controls (13).** `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`
**Layout regions.** Task header: title, status, Stop · Pinned attention/approval card · Transcript (user/Faktor turns) · Queue composer · Configuration and context-usage row
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar cockpit; pinned attention; transcript; composer; quiet config row.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Work destination; attention card opens S14; queue opens S59; Stop opens S70; Inspect/History switch destinations.
**Traceability.** Design p.40; contract p.41; controls `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`.

### S14 — Approve an action

**Group / job.** Work. The approval dock shows the requesting session/child, operation, target, reason and effect boundary without losing the running
**Primary action.** Allow Once
**Controls (7).** `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`
**Layout regions.** Approval header and requester · Exact command and target · Effect boundary: network, writable paths, requested scope · Why / AC reference · Request details disclosure · Create a Rule for Future Requests / Allow Once / Deny
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar attention/dock; exact command in monospace; one primary Allow Once.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from the pinned attention card or host notification; Allow Once/Deny resolves the exact waiter; Details expands in place.
**Traceability.** Design p.42; contract p.43; controls `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`.

### S40 — Answer a task question

**Group / job.** Work. Answer the exact current task question and record the response without changing the original task contract.
**Primary action.** Send Answer
**Controls (2).** `D-07 V-07`
**Layout regions.** Question header and waiting status · Question text and context · Answer choices and optional constraint · Send Answer / Review Task
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar question card; choices plus optional constraint.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from a question attention row; Send Answer returns to Work; Review Task opens the task contract.
**Traceability.** Design p.44; contract p.45; controls `D-07 V-07`.

### S22 — Live activity

**Group / job.** Work. A live timeline explains what is active, waiting, queued, failed or uncertain. Critical attention remains visible when details are
**Primary action.** Open Approval
**Controls (10).** `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`
**Layout regions.** Activity timeline with status dots · Queued-next row · Open Approval / Stop Task actions · Collapsed critical attention
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Overview > Activity from Work/Inspect; Open Approval opens S14; Stop Task uses S70 scope.
**Traceability.** Design p.46; contract p.47; controls `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`.

### S59 — Queued messages

**Group / job.** Work. Edit, reorder or remove only unadmitted queued inputs through an acknowledged queue revision.
**Primary action.** Save Edit
**Controls (1).** `D-05`
**Layout regions.** Queued-message list with order/status · Move Up/Move Down · Edit-message field · Save Edit / Remove
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar queue list; one edit field.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from the Work queue control; Save Edit applies to an unadmitted item; Move Up/Down reorders; back returns to Work.
**Traceability.** Design p.49; contract p.50; controls `D-05`.

### S70 — Scoped stop actions

**Group / job.** Work. Choose run cancellation, turn abort or owned-process stop and acknowledge the requested operation separately.
**Primary action.** Stop Run
**Controls (3).** `A-10 X-05 D-07`
**Layout regions.** Stop-scope dialog: run / current response / process · Consequence copy for each scope · More Options for process scope · Stop Run / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; three stop scopes; consequence copy.
**States.** Stop run/current response/process are separate; already-issued commands/network calls cannot be undone.
**Navigation.** Opened from Stop in the task header or Stop › More Options; Stop Run/Cancel returns to Work; process scope remains separate.
**Traceability.** Design p.51; contract p.52; controls `A-10 X-05 D-07`.

### S41 — Inspect overview

**Group / job.** Inspect and proof. Read all four engineering boundaries and open the selected plan, criterion, artifact or candidate from one overview.
**Primary action.** Review Missing Evidence
**Controls (4).** `V-07 V-08 V-09 V-10`
**Layout regions.** Completion summary: criteria, claims, integration, finish · Explore: plan, changed files, activity · Review Missing Evidence action
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar overview; four completion rows; explore rows.
**States.** Not complete; criterion pending; claim needs evidence; integration not started; finish pending.
**Navigation.** Inspect destination overview; rows open S42, S24, S22; Review Missing Evidence opens S45.
**Traceability.** Design p.55; contract p.56; controls `V-07 V-08 V-09 V-10`.

### S42 — Plan and dependencies

**Group / job.** Inspect and proof. Inspect the dependency-ordered plan and selected step without confusing a planned checkmark with observed proof.
**Primary action.** Open AC-03
**Controls (2).** `V-07 V-08`
**Layout regions.** Dependency-ordered step list with statuses · Selected step detail and evidence requirement · Constraints/invariants · Open AC-03
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; step list; selected detail.
**States.** Done/waiting/blocked step states; planned checkmarks are not observed proof.
**Navigation.** Overview > Plan; Open AC-03 opens S45/S46; Back returns to the exact plan row.
**Traceability.** Design p.57; contract p.58; controls `V-07 V-08`.

### S24 — Review changes

**Group / job.** Inspect and proof. Use each host’s native diff editor. Keep base/candidate identity, acceptance criteria and remaining verification adjacent to code
**Primary action.** Open Diff
**Controls (8).** `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`
**Layout regions.** Changed-file list with hunk stats and comments · Native diff preview · Verification/evidence prompt · Open Diff / Add Review Task
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Overview > Changes; Open Diff opens S43; Add Review Task creates a bounded review child; back returns to Overview.
**Traceability.** Design p.59; contract p.60; controls `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`.

### S43 — Native diff review

**Group / job.** Inspect and proof. Review the immutable base and selected candidate in the native editor with exact hunk and claim navigation.
**Primary action.** Open Related Criterion
**Controls (2).** `V-01 V-03`
**Layout regions.** Native base/candidate diff editor · Change counter and hunk navigation · Open Related Criterion · Read-only base/candidate labels
**Host variants.** VS Code: Dark Modern diff editor · VS Code diff. JetBrains: Light diff viewer · DiffManager.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native diff editor: base read-only, candidate read-only, hunk navigation, no custom chrome.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Native diff editor opened from S24; Open Related Criterion returns to S45; closing restores the previous editor/selection.
**Traceability.** Design p.61; contract p.62; controls `V-01 V-03`.

### S45 — Required verification

**Group / job.** Inspect and proof. Inspect every required criterion, its current evidence and explicit pending, failed, refused or unavailable result.
**Primary action.** Open AC-03 Details
**Controls (3).** `V-07 V-08 V-09`
**Layout regions.** Required-criteria list with result and evidence · Selected criterion detail: pending/failed/refused/unavailable · Verification basis: requirement, claim, source, candidate · Open AC-03 Details / Back to Overview
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; criteria list; selected evidence detail.
**States.** Passed / Needs evidence / Failed / Refused / Unavailable; pending stays pending.
**Navigation.** Overview > Verification or S41/S42; Open AC-03 Details opens S46; Back to Overview restores selection.
**Traceability.** Design p.63; contract p.64; controls `V-07 V-08 V-09`.

### S46 — Evidence detail

**Group / job.** Inspect and proof. Follow one criterion to its exact artifact, source, claim and candidate; open bounded excerpts and full content.
**Primary action.** Open Full Output
**Controls (2).** `V-07 V-08`
**Layout regions.** Evidence header and result · Bounded excerpt · Source path/range · Claim and candidate identity · Open Full Output
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; excerpt; source/claim/candidate.
**States.** Passed evidence with bounded excerpt; source/claim/candidate identity.
**Navigation.** Opened from S45; Open Full Output opens the bounded artifact view; back returns to the criterion.
**Traceability.** Design p.65; contract p.66; controls `V-07 V-08`.

### S47 — Proof identity mismatch

**Group / job.** Inspect and proof. Remove stale verified presentation when proof belongs to a different candidate; retain the historical record separately.
**Primary action.** Verify Current Candidate
**Controls (2).** `V-09 V-12`
**Layout regions.** Out-of-date proof banner · Verified vs current candidate identities · Criteria/claims/integration status · Full proof record disclosure · Verify Current Candidate / Open Previous Results
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; stale proof banner; identity pairs.
**States.** Verification out of date; verified c-07 vs current c-08; historical record remains separate.
**Navigation.** Overview > Proof when candidate identity changes; Verify Current Candidate re-runs required checks; Open Previous Results keeps history separate.
**Traceability.** Design p.67; contract p.68; controls `V-09 V-12`.

### S44 — Integration conflict

**Group / job.** Inspect and proof. Preserve user edits when landing preconditions change; review a new candidate and rerun affected verification.
**Primary action.** Review Conflict
**Controls (2).** `V-01 V-12`
**Layout regions.** Workspace-changed banner · Candidate vs workspace columns · Per-file conflict/waiting rows · Review Conflict / Keep Candidate
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; candidate/workspace columns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Overview > Integration when landing preconditions change; Review Conflict opens the native diff/merge; Keep Candidate abandons the new landing.
**Traceability.** Design p.69; contract p.70; controls `V-01 V-12`.

### S48 — Finish actions

**Group / job.** Inspect and proof. Inspect the hierarchical Local / Commit / Push / PR receipts and resolve only the blocked native finish step.
**Primary action.** Configure Remote…
**Controls (2).** `V-10 V-11`
**Layout regions.** Finish header and level · Hierarchical Local / Commit / Push / Pull request rows · Per-step receipt/status and blocked cause · Configure Remote… / Keep Result
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; finish ladder; receipts.
**States.** Done / Blocked / Not started; blocked push names Configure Remote…; completed steps do not rerun.
**Navigation.** Overview > Finish from S32/S65; Configure Remote… opens host SCM configuration; Keep Result returns to the outcome.
**Traceability.** Design p.71; contract p.72; controls `V-10 V-11`.

### S32 — Task outcome

**Group / job.** Inspect and proof. Finish with explicit engineering status: required acceptance, claim evidence, matching integration and requested finish.
**Primary action.** Open Required Verification
**Controls (8).** `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`
**Layout regions.** Outcome status banner · Four-row boundary summary: acceptance, claims, integration, finish · Actions: Open Required Verification, Inspect Artifacts
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; four-boundary outcome.
**States.** Not complete yet; needs evidence; acceptance/claims/integration/finish rows pending.
**Navigation.** Overview > Outcome from Work/Inspect; Open Required Verification opens S45; Inspect Artifacts opens evidence.
**Traceability.** Design p.73; contract p.74; controls `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`.

### S65 — Verified outcome

**Group / job.** Inspect and proof. Show a separate complete example only when all required acceptance, claims, matching integration and finish agree.
**Primary action.** Review Integrated Change
**Controls (2).** `V-09 V-10`
**Layout regions.** Verified-complete banner · Four-row all-pass summary · Full proof record · Review Integrated Change / New Task
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; verified outcome; proof record.
**States.** Verified complete: all four boundaries pass; proof record remains inspectable.
**Navigation.** Overview > Outcome when all four boundaries pass; Review Integrated Change opens S43; New Task starts a fresh conversation.
**Traceability.** Design p.75; contract p.76; controls `V-09 V-10`.

### S25 — Restore & fork

**Group / job.** Inspect and proof. Show recorded checkpoint identity, content coverage and branching separately. Preview the exact files and conversation range
**Primary action.** Review Restore…
**Controls (6).** `V-04 V-05 V-06 V-12 D-06 Q-09`
**Layout regions.** Checkpoint list with coverage · Restore-scope segmented control: Code / Conversation / Both · Affected-file list with readiness · Uncovered-effects warning · Review Restore… / Fork Conversation
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; checkpoint list; restore scope.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** History/Overview > Checkpoints; Review Restore… previews files; Fork Conversation creates separate lineage; back restores selection.
**Traceability.** Design p.77; contract p.78; controls `V-04 V-05 V-06 V-12 D-06 Q-09`.

### S49 — Cancelled run

**Group / job.** Inspect and proof. Retain candidate work, evidence, recorded spend and drafts after a durable cancellation; completion remains separate.
**Primary action.** Review Kept Work
**Controls (3).** `D-07 V-04 B-04`
**Layout regions.** Run-cancelled status · Kept-work summary: candidate, verification, processes, spend, outcome · Review Kept Work / New Task
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; kept-work summary.
**States.** Cancelled/stopped; candidate, verification, processes, spend and outcome kept; completion remains separate.
**Navigation.** Work/Inspect after a durable cancellation; Review Kept Work opens the candidate/diff/evidence; New Task leaves the kept work behind.
**Traceability.** Design p.79; contract p.80; controls `D-07 V-04 B-04`.

### S23 — Delegated agents

**Group / job.** Agents. Inspect parent and child work as a tree with bounded concurrency, shared spend and candidate ownership. Child output is
**Primary action.** Send Steer
**Controls (10).** `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`
**Layout regions.** Agents tree with role/status · Shared budget and concurrency summary · Steer composer · Agent details disclosure · Send Steer / Stop Verifier
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; agent tree; shared budget.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Inspect > Agents; selecting a child opens S50; approvals open S14; Compare opens S51; Coordination opens S52.
**Traceability.** Design p.82; contract p.83; controls `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`.

### S50 — Agent detail and steer

**Group / job.** Agents. Inspect a child assignment, ownership, inherited authority and shared budget before steering or stopping that child.
**Primary action.** Send Steer
**Controls (4).** `A-09 A-10 A-11 A-12`
**Layout regions.** Child identity and status · Task/workspace ownership · Allowed capabilities and inherited authority · Budget reservation · Steer field with character count · Send Steer / Stop Verifier
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; child detail; steer field.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Agents > child; Send Steer records guidance without changing task/permissions; Stop Verifier uses child-scoped stop; back returns to the tree.
**Traceability.** Design p.84; contract p.85; controls `A-09 A-10 A-11 A-12`.

### S51 — Compare approaches

**Group / job.** Agents. Compare candidate approaches against one contract and recommend without automatically integrating the winner.
**Primary action.** Review Approach B
**Controls (3).** `M-14 A-12 V-07`
**Layout regions.** Comparison cards A/B with criteria/spend · Recommendation and rationale · Comparison basis · Review Approach B / Stop Comparison
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; comparison cards.
**States.** Approach A pending (2/3); approach B recommended (3/3); choosing does not integrate.
**Navigation.** Inspect > Agents > Compare; Review Approach B inspects the chosen candidate without integrating; Stop Comparison cancels both candidates.
**Traceability.** Design p.86; contract p.87; controls `M-14 A-12 V-07`.

### S52 — Coordination

**Group / job.** Agents. Read and post bounded durable coordination notes with explicit read acknowledgement and stable revision paging.
**Primary action.** Post Note
**Controls (2).** `A-12 D-07`
**Layout regions.** Notes list with unread/author/time · New-note subject/message · Post Note / Mark All Read
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; durable notes.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Inspect > Agents > Coordination; Post Note appends to the durable board; Mark All Read acknowledges the loaded head; back returns to Agents.
**Traceability.** Design p.88; contract p.89; controls `A-12 D-07`.

### S26 — Conversation history

**Group / job.** History. History shows durable conversation identity, worktree, status and engineering outcome. Label title search separately from
**Primary action.** Open Conversation
**Controls (8).** `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`
**Layout regions.** Title search field · Conversation rows with status and worktree · Context menu: find, rename, pin, archive, fork, export, delete · Open Conversation / New Conversation
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar history list; title search.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** History destination; row opens the conversation; context menu actions stay in History; New Conversation returns to Work.
**Traceability.** Design p.91; contract p.92; controls `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`.

### S61 — Empty conversation history

**Group / job.** History. Distinguish true empty history, no search matches and failed reads; lead naturally back to a retained draft.
**Primary action.** Start a Task
**Controls (2).** `D-01 D-02`
**Layout regions.** Empty-history message · Draft-retained row · Start a Task / Open Draft
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar empty state; draft retention.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** History empty state; Start a Task returns to Work; Open Draft restores the retained unsent draft.
**Traceability.** Design p.93; contract p.94; controls `D-01 D-02`.

### S71 — Find in conversation

**Group / job.** History. Search the full durable conversation through the service cursor and return to the exact original message.
**Primary action.** Go to Message
**Controls (1).** `D-03`
**Layout regions.** Conversation search field and result count · Result rows with role/snippet · Go to Message / Back to Conversation
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar search results; result count.
**States.** Result 1 of 12; search covers unloaded messages; return to exact original message.
**Navigation.** Opened from History/Conversation Find or Ctrl/Cmd+F; Go to Message returns to the exact original message; Back returns to the conversation.
**Traceability.** Design p.96; contract p.97; controls `D-03`.

### S72 — Switch conversation safely

**Group / job.** History. Preserve draft, caret, scroll, sources and owned work while rebinding every response and action to another conversation.
**Primary action.** Switch
**Controls (3).** `D-01 D-15 X-13`
**Layout regions.** Switch-conversation confirmation · Target conversation identity · Draft/run retention facts · Switch / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; draft/run retention facts.
**States.** Draft/attachments/caret/scroll saved; run 047 keeps running in the other conversation.
**Navigation.** Opened from the conversation switcher; Switch rebinds the view; Cancel keeps the current conversation; the other run keeps running.
**Traceability.** Design p.98; contract p.99; controls `D-01 D-15 X-13`.

### S53 — Recorded outcome cohort

**Group / job.** Usage. Inspect an outcome cohort with eligible attempted spend, verified landed denominator and actual local/hosted routes.
**Primary action.** Open Runs
**Controls (3).** `O-06 K-12 M-13`
**Layout regions.** Cohort header and time window · Cost per verified change · Failed/cancelled attempts and spend · Local vs hosted routes · Calculation method disclosure · Open Runs / Export…
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage cohort; denominator explanation.
**States.** Unavailable when no verified changes (never $0); failed/cancelled attempts included; local/hosted shown separately.
**Navigation.** Usage > Outcomes; Open Runs opens the recorded run cohort; Export… exports the bounded cohort.
**Traceability.** Design p.101; contract p.102; controls `O-06 K-12 M-13`.

### S54 — Usage and quota

**Group / job.** Usage. Separate provider API spend, optional subscription quota, reservations and unmeasured local operating cost.
**Primary action.** None (read-only)
**Controls (2).** `O-06 O-07`
**Layout regions.** Usage categories: local inference, provider API, subscription quota, GPU cost · No-account/unavailable labels · Account Settings…
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage categories; read-only.
**States.** No account needed for local work; subscription quota and GPU cost unavailable, not zero.
**Navigation.** Usage > Account; read-only; Account Settings… opens the optional control-plane account screen (S55).
**Traceability.** Design p.103; contract p.104; controls `O-06 O-07`.

### S27 — Usage & outcomes

**Group / job.** Usage. Separate token accounting, provider/cache prices, quota, local estimates and verification-based engineering cost. Explain every
**Primary action.** Inspect Runs
**Controls (6).** `B-04 M-13 K-12 O-06 V-09 D-07`
**Layout regions.** Usage summary and cost per verified change · Token/cache breakdown · Provider quota · How costs are calculated · Inspect Runs / Export…
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage summary; token/cost breakdown.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Usage destination; Inspect Runs opens S53; Export… exports the bounded usage view.
**Traceability.** Design p.105; contract p.106; controls `B-04 M-13 K-12 O-06 V-09 D-07`.

### S02 — Choose a model

**Group / job.** Models and providers. Search by exact ID, provider or capability. A selected item exposes authority, limits and availability before selection; favorite and
**Primary action.** Enter selects
**Controls (14).** `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`
**Layout regions.** Search field · Current model group · Favorites/recent groups · Served-model list with capability/authority · Unavailable model rows with reason · Selected-model detail; apply timing
**Host variants.** VS Code: Dark Modern Quick Pick · native. JetBrains: Light popup chooser · native.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser; 32 px rows; Enter/Esc/Tab host focus.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from the Work model control or Settings > Faktor > Models; Enter selects, Escape cancels and restores the previous pair; capability details expand in place.
**Traceability.** Design p.108; contract p.109; controls `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`.

### S03 — Provider connection

**Group / job.** Models and providers. Connections have a stable instance ID, API family, endpoint and credential reference. Test auth, model discovery and capability
**Primary action.** Save Connection · JetBrains: OK / Apply
**Controls (10).** `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`
**Layout regions.** Connection name and instance ID · API family picker · Base URL and resolved host · Credential reference and custom headers · Connection test/health status · Save Connection / Test Connection
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; label/value rows; Test Connection stays adjacent.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Providers > connection; Save Connection applies one validated patch; Test Connection probes without saving; OK/Apply returns to Settings.
**Traceability.** Design p.110; contract p.111; controls `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`.

### S04 — Local inference

**Group / job.** Models and providers. Locality is relative to the daemon. Explain loopback, allocation, model readiness and resource ownership before offering local-
**Primary action.** Save · JetBrains: OK / Apply
**Controls (7).** `P-07 P-08 P-09 P-13 P-15 C-10 M-09`
**Layout regions.** Adapter and endpoint · Locality explanation · Context allocation / keep-loaded policy · Loopback and private-network controls · Embedding model and retrieval fallback · Probe Model / Save
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; label/value rows; locality and allocation shown before runtime controls.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Providers > local connection; Save applies; Probe Model observes without certifying coding quality; OK/Apply returns to Settings.
**Traceability.** Design p.112; contract p.113; controls `P-07 P-08 P-09 P-13 P-15 C-10 M-09`.

### S05 — Model tuning

**Group / job.** Models and providers. A capability-driven form exposes standard generation controls without pretending every provider supports every field. Unset is
**Primary action.** Apply Tuning · JetBrains: OK / Apply
**Controls (14).** `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`
**Layout regions.** Model identity and apply timing · Reasoning effort · Maximum output · Temperature / top-p · Advanced parameters disclosure · Effective-request preview · Apply Tuning / Reset
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; reasoning segmented control; advanced parameters behind disclosure.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Model Tuning or the Work model detail; Apply Tuning applies to the next call; Reset removes overrides; Cancel preserves staged edits.
**Traceability.** Design p.114; contract p.115; controls `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`.

### S63 — Compare exact models

**Group / job.** Models and providers. Compare exact provider/model pairs with held-constant task and context, separate candidates and shared spend.
**Primary action.** Start Comparison
**Controls (3).** `M-14 B-01 B-02`
**Layout regions.** Two exact model candidates with data boundary · Shared cap · Comparison scope · Start Comparison / Cancel
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from Work/Inspect Compare models; Start Comparison admits a bounded comparison; Cancel leaves the draft unchanged.
**Traceability.** Design p.116; contract p.117; controls `M-14 B-01 B-02`.

### S06 — Choose an agent

**Group / job.** Models and providers. Code, Ask, Plan, Debug and Review are proposed task envelopes with visible instructions, models, tool limits and delegation policy.
**Primary action.** Enter selects
**Controls (12).** `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`
**Layout regions.** Agent search · Code / Ask / Plan / Debug / Review rows · Selected agent description, model, permissions · New Custom Agent…
**Host variants.** VS Code: Dark Modern Quick Pick · native. JetBrains: Light popup chooser · native.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser; 32 px rows; selected detail pane.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from the Work agent control; Enter selects; New Custom Agent… opens S07; Escape restores the previous preset.
**Traceability.** Design p.118; contract p.119; controls `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`.

### S07 — Custom agent

**Group / job.** Models and providers. Build a role from bounded instructions, model settings, tools and parent-owned budget. Preview the resulting envelope before
**Primary action.** Save Agent · JetBrains: OK / Apply
**Controls (9).** `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`
**Layout regions.** Agent name and instructions · Model and tools · Concurrency and per-agent budget · Delegation policy · Save Agent / Cancel
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; bounded instruction field; envelope preview.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Agents > agent; Save Agent previews the envelope and saves; Cancel keeps the previous preset.
**Traceability.** Design p.120; contract p.121; controls `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`.

### S08 — Context

**Group / job.** Context. Inspect what enters a call, what it costs in estimated or observed tokens, and where it came from. Capacity and input usage have
**Primary action.** Open Sources
**Controls (12).** `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`
**Layout regions.** Sent / reserved / capacity header · Category breakdown: system, rules, tools, conversation, files, retrieval · Measurement basis and safety reserve · Open Sources / Export…
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Overview > Context or composer + Context; Open Sources opens S09; Export… exports the bounded context view; Back to Work returns.
**Traceability.** Design p.123; contract p.124; controls `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`.

### S62 — Unavailable context usage

**Group / job.** Context. Render null authoritative context usage as unavailable and keep any draft estimate on a separate basis.
**Primary action.** Open Sources
**Controls (2).** `C-01 C-02`
**Layout regions.** Last-call header · Sent: unavailable · Model capacity · Draft estimate on a separate basis · Open Sources / Back to Work
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Last-call usage unavailable; no percentage guess; draft estimate is labelled separately.
**Navigation.** Overview > Context when usage is null; Open Sources opens S09; Back to Work returns without inventing a percentage.
**Traceability.** Design p.125; contract p.126; controls `C-01 C-02`.

### S09 — Add context

**Group / job.** Context. A single discoverable picker adds exact files, ranges, diagnostics, terminal excerpts, diffs, URLs and prior-chat references to a
**Primary action.** Enter selects · Add 3 Items
**Controls (9).** `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`
**Layout regions.** Search field · Source rows with type/range/status · Selection count · Unsupported-image explanation · Add 3 Items
**Host variants.** VS Code: Dark Modern Quick Pick · native. JetBrains: Light popup chooser · native.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser with multi-select; 32 px rows.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from composer + Context or S08 Open Sources; Enter selects, Add 3 Items adds; Escape cancels and preserves the draft.
**Traceability.** Design p.127; contract p.128; controls `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`.

### S10 — Compact context

**Group / job.** Context. Compaction preserves task obligations and operation identity while reducing eligible history. Show a plan before dispatch and a
**Primary action.** Compact
**Controls (7).** `K-01 K-02 K-03 K-04 C-12 I-11 V-07`
**Layout regions.** Now vs after estimate · Always-kept list: task, criteria, constraints, candidate, evidence, permissions, approvals · Summary-model route and pricing/data boundary · Compact / Show Details / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from Context/compaction threshold Review; Compact starts a durable operation; Show Details expands always-kept facts; Cancel keeps the prior basis.
**Traceability.** Design p.129; contract p.130; controls `K-01 K-02 K-03 K-04 C-12 I-11 V-07`.

### S11 — Instructions & rules

**Group / job.** Instructions and memory. Inspect instructions by origin and activation scope, edit allowed sources and preview the merged bundle that the next task will
**Primary action.** Add Rule… · JetBrains: OK / Apply
**Controls (11).** `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`
**Layout regions.** Active instructions grouped by origin/scope · Output style · Preview Combined Instructions · Add Rule… / Import Rules…
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; origin/scope rows; merged preview disclosure.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Instructions; Add Rule/Import open bounded editors; Preview Combined Instructions opens the merged bundle; OK/Apply saves.
**Traceability.** Design p.133; contract p.134; controls `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`.

### S69 — Instruction precedence

**Group / job.** Instructions and memory. Explain instruction precedence and managed policy conflicts without treating project text as executable authority.
**Primary action.** Open Project Instructions · JetBrains: OK / Apply
**Controls (3).** `I-04 I-08 Q-12`
**Layout regions.** Precedence ladder (highest first) · Locked/conflict markers · Policy explanation and resolution · Open Project Instructions / Back
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; precedence ladder; locked/conflict rows.
**States.** Organization policy locked; project instruction conflicts; blocked upload does not run.
**Navigation.** Settings > Faktor > Instructions > Precedence; Open Project Instructions opens the source; Back returns to the precedence ladder.
**Traceability.** Design p.135; contract p.136; controls `I-04 I-08 Q-12`.

### S12 — Memory

**Group / job.** Instructions and memory. Separate learned failure priors, user instructions and proposed personal/project memory. Each record has scope, source, date
**Primary action.** Save Memory · JetBrains: OK / Apply
**Controls (5).** `K-05 K-10 K-11 O-09 I-11`
**Layout regions.** New-memory policy · Saved memory rows with source/date · Deletion semantics · Save Memory / Delete…
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; versioned memory rows; delete disclosure.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Memory; Save Memory persists; Delete… opens S66; OK/Apply returns to Settings.
**Traceability.** Design p.137; contract p.138; controls `K-05 K-10 K-11 O-09 I-11`.

### S66 — Memory deletion

**Group / job.** Instructions and memory. Delete one versioned memory from future retrieval while disclosing earlier prompts, artifacts and log copies.
**Primary action.** Delete
**Controls (2).** `K-10 K-11`
**Layout regions.** Delete-memory confirmation · Scope and future-retrieval effect · Earlier prompt/artifact/log copies warning · Delete / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; delete confirmation names the object.
**States.** Delete stops future use; earlier prompts/artifacts/logs are not removed; user memory/failure learning unaffected.
**Navigation.** Opened from S12 Delete…; Delete removes future retrieval only; Cancel returns to Memory.
**Traceability.** Design p.139; contract p.140; controls `K-10 K-11`.

### S13 — Permissions

**Group / job.** Permissions and tools. Explain effective permissions by tool, action, target and scope. Show persistent rules separately from current pending approvals
**Primary action.** Apply Policy · JetBrains: OK / Apply
**Controls (13).** `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`
**Layout regions.** Permission preset · Per-action rows: read, edit candidate, run shell, write outside workspace · Files/network containment · Organization lock explanation · Test a Rule… / Apply Policy
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; action/target/scope matrix; locked rows.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Permissions; Apply Policy saves a validated policy; Test a Rule opens the simulator; organization locks remain visible.
**Traceability.** Design p.142; contract p.143; controls `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`.

### S15 — Tools & MCP

**Group / job.** Permissions and tools. Group built-in tools and MCP servers by origin, availability and active permissions. Make discovered, enabled, healthy and
**Primary action.** Add Server… · JetBrains: OK / Apply
**Controls (13).** `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`
**Layout regions.** Server list with health/auth/tool count · Selected server tools with allow/deny · Resources and prompts · Token cost of definitions · Add Server… / Open Logs
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; server health rows; tool policy rows.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Tools & MCP; Add Server opens S16; server rows expose health/auth/tools; Open Logs opens the bounded log view.
**Traceability.** Design p.144; contract p.145; controls `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`.

### S16 — Add MCP server

**Group / job.** Permissions and tools. Configure a supervised stdio server with exact command/args and explicit trust. Remote transport is offered only after capability
**Primary action.** Save and Start
**Controls (8).** `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`
**Layout regions.** Transport stdio/HTTP · Name, command, arguments, environment · Trust statement · Save and Start / Cancel
**Host variants.** VS Code: Dark Modern editor tab · webview. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor/dialog; stdio/HTTP segmented control; command/args fields.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Opened from S15 Add Server…; Save and Start saves the spec then explicitly starts; Cancel keeps the previous server config.
**Traceability.** Design p.146; contract p.147; controls `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`.

### S68 — Future MCP authentication

**Group / job.** Permissions and tools. Specify future remote MCP authentication with exact origin, scopes, token storage and cancellation behavior.
**Primary action.** Open Sign-In Page
**Controls (2).** `T-04 T-06`
**Layout regions.** Server origin and scopes · Read/write access · Token storage (machine vault) · Browser sign-in explanation · Open Sign-In Page / Cancel
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; origin/scopes/token storage.
**States.** Sign-in pending/cancelled; token stored in machine vault; cancellation keeps server settings.
**Navigation.** Opened from S15 server Sign In…; Open Sign-In Page starts browser auth; Cancel keeps server settings and does not connect.
**Traceability.** Design p.148; contract p.149; controls `T-04 T-06`.

### S17 — Skills & commands

**Group / job.** Permissions and tools. Discover reusable workflows without confusing installation with permission grants. Commands say whether they insert text, start
**Primary action.** Install Skill… · JetBrains: OK / Apply
**Controls (8).** `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`
**Layout regions.** Skills list with scope/invocation · Commands list with action type · New Command… · Install Skill…
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; skill and command lists.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Skills & Hooks; Install Skill… and New Command… open bounded editors; OK/Apply saves.
**Traceability.** Design p.150; contract p.151; controls `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`.

### S18 — Lifecycle hook

**Group / job.** Permissions and tools. Edit exact native lifecycle events, executable arguments, environment, bounds and failure policy. Test under the same capability
**Primary action.** Save Hook · JetBrains: OK / Apply
**Controls (8).** `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`
**Layout regions.** Hook event · Command · Timeout and output limit · Failure policy · Allowed environment variables · Last test result · Save Hook / Test Hook
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; event, command, bounds and failure policy.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Skills & Hooks > hook; Test Hook opens S67; Save Hook saves a validated version; Cancel keeps the previous hook.
**Traceability.** Design p.152; contract p.153; controls `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`.

### S67 — Hook test result

**Group / job.** Permissions and tools. Inspect a real bounded hook test, deadline, output and verdict without converting successful output into authority.
**Primary action.** Back to Hook
**Controls (2).** `S-12 S-13`
**Layout regions.** Result and exit code · Time and policy verdict · Bounded output · Back to Hook / Show Output / Run Again
**Host variants.** VS Code: Dark Modern modal message · native. JetBrains: Light DIALOGWRAPPER.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Passed exit 0 in 128 ms; policy unchanged; passing hook cannot grant authority.
**Navigation.** Opened from S18 Test Hook; Back to Hook returns to the editor; Run Again repeats the bounded test.
**Traceability.** Design p.154; contract p.155; controls `S-12 S-13`.

### S19 — Terminal

**Group / job.** Permissions and tools. An owned process table and bounded output coexist with the task transcript. Terminal kill, run stop and session abort are separate
**Primary action.** Open Full Output
**Controls (8).** `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`
**Layout regions.** Terminal tabs and owned process list · Bounded output · Full-output link · Stop Process · Filesystem/network scope
**Host variants.** VS Code: Dark Modern panel terminal · native. JetBrains: Light tool window · console.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native terminal/console panel; bounded output; process tabs; 64 KiB visible tail.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Host terminal/console surface owned by the task; Open Full Output opens the bounded artifact; Stop Process acts only on the owned process.
**Traceability.** Design p.156; contract p.157; controls `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`.

### S20 — Browser

**Group / job.** Permissions and tools. Integrate host diagnostics and explicit code actions. Browser controls require an actual advertised adapter with account, machine
**Primary action.** Capture Screenshot
**Controls (8).** `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`
**Layout regions.** Preview URL · Rendered page · Console/network counts · Capture Screenshot / Add Diagnostics
**Host variants.** VS Code: Dark Modern simple browser · native. JetBrains: Light JCEF preview window.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Host browser/preview surface opened by an explicit adapter action; Capture Screenshot produces task evidence; Add Diagnostics opens host diagnostics.
**Traceability.** Design p.158; contract p.159; controls `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`.

### S21 — Budgets & retries

**Group / job.** Settings and diagnostics. Show exact run limits and current accounting beside reliability controls. A soft warning is different from enforced admission
**Primary action.** Apply Defaults · JetBrains: OK / Apply
**Controls (12).** `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`
**Layout regions.** Defaults vs running-task limits · Token limit and spending limit · Spent vs held · Retry attempts and backoff · Advanced reliability disclosure · Apply Defaults / Reset
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; caps and retry fields; held/spent row.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Budgets; Apply Defaults applies to new tasks; Reset restores defaults; running tasks keep captured limits.
**Traceability.** Design p.161; contract p.162; controls `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`.

### S28 — Efficiency

**Group / job.** Settings and diagnostics. Expose current production efficiency flags and their off-switches with honest measurement, fallback and apply timing.
**Primary action.** Apply · JetBrains: OK / Apply
**Controls (12).** `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`
**Layout regions.** Efficiency flags with explanations · Routing policy · Search without embeddings · Apply / Reset to Defaults
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; flags with explanations.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Efficiency; Apply changes routing flags; Reset to Defaults restores production defaults; no fake measured effect.
**Traceability.** Design p.163; contract p.164; controls `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`.

### S29 — General settings

**Group / job.** Settings and diagnostics. Search settings across allowed scopes and inspect each effective value, source, lock and timing. Distinguish staged, saved and
**Primary action.** VS Code: Settings saves valid values · JetBrains: OK / Apply
**Controls (11).** `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`
**Layout regions.** Native settings search · Settings list with effective values · Scope/lock/timing · OK / Cancel / Apply
**Host variants.** VS Code: Dark Modern settings editor · native. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native Settings editor; search field; scope/provenance list.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Native Settings > Faktor or Settings > Faktor; OK/Apply commits valid values; staged/saved/runtime-applied states remain distinct.
**Traceability.** Design p.165; contract p.166; controls `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`.

### S64 — Settings search and provenance

**Group / job.** Settings and diagnostics. Find persistent settings in the native host, inspect scope/provenance and remove an override deliberately.
**Primary action.** VS Code: Settings saves valid values · JetBrains: OK / Apply
**Controls (2).** `O-01 D-11`
**Layout regions.** Settings search results · Provenance: user/workspace/organization · Override/reset action · OK / Apply
**Host variants.** VS Code: Dark Modern settings editor · native. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native Settings editor; search results; provenance and reset.
**States.** 3 settings found; workspace override vs user default; organization lock; reset to inherited.
**Navigation.** Native Settings search; selecting a result opens its provenance; Reset to Inherited Value removes the override.
**Traceability.** Design p.167; contract p.168; controls `O-01 D-11`.

### S60 — Settings revision conflict

**Group / job.** Settings and diagnostics. Resolve a rejected configuration patch against the new effective revision without losing staged edits.
**Primary action.** Save Resolved Settings · JetBrains: OK / Apply
**Controls (2).** `O-01 M-12`
**Layout regions.** Revision-conflict banner · Your value vs now-saved value · Keep my value / Use saved value · Save Resolved Settings / Cancel
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; conflict banner; per-field choice.
**States.** Settings changed while editing; staged values kept; conflicting field must be resolved.
**Navigation.** Opened when a settings patch conflicts; Save Resolved Settings applies the chosen values; Cancel keeps staged edits.
**Traceability.** Design p.169; contract p.170; controls `O-01 M-12`.

### S55 — Optional control-plane account

**Group / job.** Settings and diagnostics. Connect a machine-scoped optional account with write-only secrets while local coding remains independent.
**Primary action.** Connect Account · JetBrains: OK / Apply
**Controls (2).** `O-07 P-04`
**Layout regions.** Optional-account explanation · Server and organization · Write-only token reference · Connection status · Connect Account / Cancel
**Host variants.** VS Code: Dark Modern editor tab · webview SETTINGS. JetBrains: Light settings dialog · Configurable.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; optional account fields; write-only token.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Settings > Faktor > Account; Connect Account starts the optional control-plane flow; Cancel keeps local-only operation.
**Traceability.** Design p.171; contract p.172; controls `O-07 P-04`.

### S30 — Diagnostics

**Group / job.** Settings and diagnostics. Show readiness, version compatibility, runtime identity and bounded diagnostics with an actionable typed cause.
**Primary action.** Preview Export…
**Controls (9).** `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`
**Layout regions.** Service/version status · Executable/checksum/signature · Model provider state · Code index state · Export options: versions, capabilities, error excerpts, source content · Preview / Export… / Open Logs
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar diagnostics; export options.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Diagnostics from the Work status/More menu or Settings > Faktor > Diagnostics; Preview/Export… open bounded artifacts; Open Logs opens host logs.
**Traceability.** Design p.173; contract p.174; controls `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`.

### S56 — Protocol-blocked recovery

**Group / job.** Settings and diagnostics. Preserve the last coherent snapshot and cursor when a typed stream refusal pauses automatic recovery.
**Primary action.** Restart Service…
**Controls (2).** `O-04 D-13`
**Layout regions.** Out-of-sync banner · Last update and draft retention · Run view age · Expected/received protocol versions · Restart Service… / Copy Diagnostics
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar protocol recovery; expected/received versions.
**States.** Live updates paused; last coherent snapshot/cursor retained; run view out of date; restart is explicit.
**Navigation.** Opened from the stale/live-updates-paused banner; Restart Service… restarts explicitly; Copy Diagnostics copies the typed mismatch.
**Traceability.** Design p.175; contract p.176; controls `O-04 D-13`.

### S31 — Workspace binding

**Group / job.** Settings and diagnostics. Display editor, execution, model, vault and candidate locations together. Remote workspaces must not blur local/hosted
**Primary action.** Review New Setup
**Controls (8).** `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`
**Layout regions.** Workspace-moved banner · Where things run: editor, workspace, model endpoint, credentials · Review New Setup / Cancel
**Host variants.** VS Code: Dark Modern sidebar view · webview. JetBrains: Light tool window · Swing/Kotlin UI DSL.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar workspace binding; where things run.
**States.** Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).
**Navigation.** Diagnostics > Where things run when the workspace binding changes; Review New Setup opens S34; Cancel keeps the old binding marked stale.
**Traceability.** Design p.177; contract p.178; controls `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`.

### S58 — Narrow Work panel

**Group / job.** Settings and diagnostics. Keep a 320 px Work panel readable with stacked controls, one selected detail and an independently reachable composer.
**Primary action.** Queue Message (composer)
**Controls (3).** `D-08 D-09 D-14`
**Layout regions.** 320 px task header/status · One selected detail · Stacked configuration row · Composer remains reachable
**Host variants.** VS Code: Dark Modern sidebar view at 320 px. JetBrains: Light tool window at 320 px.
**Visual.** 4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. 320 px Work panel; stacked controls; primary/attention/composer remain visible; 200% text target.
**States.** Narrow 320 px: one selected detail, stacked configuration, composer independently reachable.
**Navigation.** Work at 320 px; the same navigation as S01, with one selected detail and the composer reachable.
**Traceability.** Design p.179; contract p.180; controls `D-08 D-09 D-14`.

## Screen-level ambiguities

- The PDF draws every screen in two themes on the spread (VS Code Dark Modern, JetBrains Light) but states that four host themes are built (VS Code light/dark, JetBrains light/dark, high contrast). The plates are reference renderings, not product screenshots (p.7).
- The screen count is 72, but the PDF numbers them S01–S72 non-sequentially in spread order; the spread order above is authoritative for navigation.
- Host applicability is both for every screen; the exact host surface differs (sidebar, editor tab, settings editor, Quick Pick, popup, DialogWrapper, diff, terminal, console, browser).
- No screen has a full native implementation in the current tree: the repository implements the Edition 5 32-screen registry and control center; the 40 Rev8-added surfaces (S33–S72) are covered at most partially by existing panels/dialogs, with no Rev8 screen registry or plate/layout contract (see `backlog.md`).
