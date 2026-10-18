# Edition 5 screens (S01–S32)

Source: `Faktor_IDE_UI_UX_2.pdf`, screen interaction contracts pp.101–108, native visual plates pp.109–172, product IA pp.007–008, responsive rules p.023 and spacing rules p.035. All 32 screens are rendered for both hosts; the PDF states "These contracts apply to both host variants" on each contract page (pp.101–108).

## Global visual contract (applies to every screen)

- Base unit 4 logical px; inline gaps 4/8; control gap 8; helper gap 4; form-row gap 12; group gap 24; panel padding 12 compact / 16 standard / 24 wide (p.035).
- Controls target 32 logical px (compact 28 where host convention allows); primary composer action 32–44; body line-height 1.4–1.5; labels 1.25–1.35; never clip text to a fixed single-line box (p.035).
- Settings use a bounded label column `minmax(10rem, 15rem)` plus a flexible value column only when there is room; stack below the measured breakpoint; `min-width:0` and `overflow-wrap:anywhere` for paths/IDs (pp.035, 050).
- Breakpoints: 240–319 px labelled vertical stack with Send/Stop and attention visible; 320–479 px two compact wrapped rows; ≥480 px full configuration summary (p.023).
- Inherit host font family/size/zoom, semantic colors/focus/high-contrast tokens; no brand font in IDE reading surfaces; no blinking/decorative motion; reduced motion removes nonessential transitions (pp.023, 035, 050).
- VS Code host tokens: `--s1:4px --s2:8px --s3:12px --s4:16px --s6:24px --s8:32px`, `--control-min:32px`, `--field-gap:12px`, `--panel-inset:12px`; JetBrains uses JBUI/UIManager metrics (p.050).
- Every screen must cover empty, loading, stale and error states; loading is bounded/cancellable; stale keeps the last coherent data with age; errors name the typed cause, affected scope and safe recovery and preserve edits (p.013).
- Optimistic actions are limited to expansion/filter/favorite/draft editing; Saved, Connected, Allowed, Cancelled, Restored, Verified and PR created require their own acknowledgements (p.013).

## Host variants

- **VS Code:** Faktor activity view with Work / Inspect / History; editor-sized `Faktor: <Category>` settings tab with left configuration nav and `Reset` / `Apply changes` staged footer; native QuickPick for one-dimensional choices; native diff editor for review; wide details open in editor tabs (pp.022, 101–108, 109–172).
- **JetBrains:** movable/dockable Faktor tool window with the same Work / Inspect / History destinations; `Settings › Tools › Faktor` Configurable with left settings tree and `Cancel / Apply / OK`; ActionSystem toolbar, native lists/dialogs and native editor diff (pp.022, 101–108, 109–172).

## S01 — Work: a complete agent cockpit

**Purpose.** Compact cockpit exposes intent, model, effort, authority, context, caps and pending attention beside the draft; Send captures an immutable configuration summary and task contract.

**Action.** Open Task options without losing the draft; Send captures an immutable configuration summary and task contract.

**Recovery.** While disconnected, retain the draft and last coherent transcript; mark observations stale and disable admission until the session is reconciled.

**Controls.** `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`

**Layout regions.** Identity/header (task, model, effort, authority, context, caps), transcript/body, attention rail, composer, status row.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: While disconnected, retain the draft and last coherent transcript; mark observations stale and disable admission until the session is reconciled.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W03, W04, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.101; plates pp.109–110; controls `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`.

## S02 — Model picker and capability details

**Purpose.** Search by exact ID, provider or capability; expose authority, limits and availability before selection; favorites/recent are user-local; show New conversation when the current session cannot change safely.

**Action.** Choose a served model and commit its provider/model pair; show New conversation when this session cannot change safely.

**Recovery.** An unknown or unavailable model remains visible with a reason; do not silently substitute it; a blocked tool capability prevents tool-using admission.

**Controls.** `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`

**Layout regions.** Search field, pinned/favorites/recent groups, served-model list with authority/limits/availability, selected-model detail, select/cancel row.

**VS Code variant.** QuickPick-style host picker over Work; Enter selects, arrows navigate, Esc cancels and preserves the previous pair/draft; Tab keeps normal focus.

**JetBrains variant.** Native dialog/list picker over the tool window; Enter selects, Esc cancels; the same requested-vs-actual and unavailable labels.

**Visual.** 32 px rows, 8 px item gap, full-width search, selected-detail pane; 240 px vertical stack; Enter/Esc/Tab use host focus rules.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: An unknown or unavailable model remains visible with a reason; do not silently substitute it; a blocked tool capability prevents tool-using admission.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W01, W03. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.101; plates pp.111–112; controls `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`.

## S03 — Provider connection editor

**Purpose.** Connections have a stable instance ID, API family, endpoint and credential reference; test auth, model discovery and capability support as separate stages; save only after secure binding.

**Action.** Validate a staged draft, preview the diff and save only after secure binding succeeds; keep the last valid connection when validation fails.

**Recovery.** A 401 offers Rotate credential; blocked destination shows endpoint policy; discovery failure keeps manually declared model IDs intact.

**Controls.** `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`

**Layout regions.** Connection list, identity/API family, endpoint and credential reference, staged test stages (reachability/auth/catalog/capability), diff/apply footer.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A 401 offers Rotate credential; blocked destination shows endpoint policy; discovery failure keeps manually declared model IDs intact.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W01. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.101; plates pp.113–114; controls `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`.

## S04 — Local model setup and runtime allocation

**Purpose.** Locality is relative to the daemon; explain loopback, allocation, model readiness and resource ownership before offering local-runtime controls.

**Action.** Test the exact endpoint from the daemon host, then show configured and observed allocation side by side.

**Recovery.** A private LAN endpoint receives a typed refusal; allow_loopback cannot authorize it; unsupported unload/allocation controls stay unavailable.

**Controls.** `P-07 P-08 P-09 P-13 P-15 C-10 M-09`

**Layout regions.** Execution location, local adapter, endpoint/test, declared vs observed allocation, embedding boundary, context preview.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A private LAN endpoint receives a typed refusal; allow_loopback cannot authorize it; unsupported unload/allocation controls stay unavailable.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W02. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.101; plates pp.115–116; controls `P-07 P-08 P-09 P-13 P-15 C-10 M-09`.

## S05 — Model tuning: reasoning and generation

**Purpose.** Capability-driven form exposes standard generation controls without pretending every provider supports every field; Unset is distinct from zero; Preview shows the normalized request and apply timing.

**Action.** Reset removes an override; Preview shows the normalized request and apply timing; changes apply only after a supported acknowledgement.

**Recovery.** Unsupported values fail locally before admission; changing the model revalidates the complete tuning bundle and retains incompatible edits for review.

**Controls.** `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`

**Layout regions.** Model identity, reasoning, thinking budget, sampling, output limit, advanced fields, reset/preview/apply bar.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unsupported values fail locally before admission; changing the model revalidates the complete tuning bundle and retains incompatible edits for review.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W01, W03. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.102; plates pp.117–118; controls `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`.

## S06 — Agent presets and execution envelopes

**Purpose.** Code, Ask, Plan, Debug and Review are proposed task envelopes with visible instructions, models, tool limits and delegation policy; a preset cannot override managed denies.

**Action.** Select or duplicate a preset, then inspect resolved permissions before applying it to a new task.

**Recovery.** A prompt-only Ask route is labelled accordingly until read-only dispatch is enforced; a preset cannot override managed denies.

**Controls.** `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`

**Layout regions.** Preset library, preset editor, resolved permissions/envelope, duplicate/apply actions.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A prompt-only Ask route is labelled accordingly until read-only dispatch is enforced; a preset cannot override managed denies.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W04. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.102; plates pp.119–120; controls `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`.

## S07 — Custom agent and delegation settings

**Purpose.** Build a role from bounded instructions, model settings, tools and parent-owned budget; preview the resulting envelope before saving; active children retain their admitted version.

**Action.** Save a validated version for the next task; an active child retains its admitted version until a supported control changes it.

**Recovery.** Duplicate names do not collide with stable IDs; invalid capability/model combinations stay in the draft and cannot be applied.

**Controls.** `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`

**Layout regions.** Role/instructions, model/reasoning, tool allowlist, delegation limits, parent budget, envelope preview.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Duplicate names do not collide with stable IDs; invalid capability/model combinations stay in the draft and cannot be applied.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W04, W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.102; plates pp.121–122; controls `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`.

## S08 — Context inspector and capacity accounting

**Purpose.** Inspect what enters a call, its estimated/observed token cost and provenance; capacity and input usage have different bases; overflow must be resolved before sending.

**Action.** Pin, exclude or open an attributed source; resolve overflow before sending; show the preview version admission will capture.

**Recovery.** With current contextUsage=null, measured usage reads Unavailable; estimates remain useful but never masquerade as a durable runtime meter.

**Controls.** `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`

**Layout regions.** Capacity totals, category accounting, source manifest, overflow actions, preview/capture bar.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: With current contextUsage=null, measured usage reads Unavailable; estimates remain useful but never masquerade as a durable runtime meter.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W02, W05, W06. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.102; plates pp.123–124; controls `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`.

## S09 — Mentions, attachments and explicit context

**Purpose.** One discoverable picker adds exact files, ranges, diagnostics, terminal excerpts, diffs, URLs and prior-chat references to a draft; failed items stay visible and removable.

**Action.** Select a source to preview resolved URI, range, version, bytes and inclusion state; Escape closes the picker without clearing text.

**Recovery.** Oversize, stale or unsupported attachments stay visible with a reason and Remove/Retry; do not silently omit a failed item on Send.

**Controls.** `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`

**Layout regions.** Search/mention field, grouped sources (files, symbols, diagnostics, terminal, diffs, URLs, prior chats), source preview, selected attachment chips.

**VS Code variant.** QuickPick-style host picker over Work; Enter selects, arrows navigate, Esc cancels and preserves the previous pair/draft; Tab keeps normal focus.

**JetBrains variant.** Native dialog/list picker over the tool window; Enter selects, Esc cancels; the same requested-vs-actual and unavailable labels.

**Visual.** 32 px rows, 8 px item gap, full-width search, selected-detail pane; 240 px vertical stack; Enter/Esc/Tab use host focus rules.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Oversize, stale or unsupported attachments stay visible with a reason and Remove/Retry; do not silently omit a failed item on Send.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W05. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.103; plates pp.125–126; controls `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`.

## S10 — Compaction and continuity review

**Purpose.** Compaction preserves task obligations and operation identity while reducing eligible history; show a plan before dispatch and a measured result afterwards.

**Action.** Review retained facts and model/egress/cost implications, then Compact using a new durable operation contract.

**Recovery.** Failure or insufficient reduction keeps the prior valid basis; pending approvals and unknown external effects cannot be erased by summarization.

**Controls.** `K-01 K-02 K-03 K-04 C-12 I-11 V-07`

**Layout regions.** Freshness/basis, retained-facts manifest, summary model/egress/cost, before/after reduction, compact/cancel actions.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Failure or insufficient reduction keeps the prior valid basis; pending approvals and unknown external effects cannot be erased by summarization.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W05, W06. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.103; plates pp.127–128; controls `K-01 K-02 K-03 K-04 C-12 I-11 V-07`.

## S11 — Rules, instructions and effective prompt

**Purpose.** Inspect instructions by origin and activation scope, edit allowed sources and preview the merged bundle the next task will capture; a policy lock is not overridable by text.

**Action.** Test a scoped rule against sample files, review conflicts and save a version; show a policy lock rather than pretending text can override it.

**Recovery.** An invalid glob or unresolved template variable blocks Apply; unsaved edits survive navigation and are never mistaken for applied runtime state.

**Controls.** `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`

**Layout regions.** Source tree, instruction editor, activation/match preview, merged effective prompt, conflict/lock inspector.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: An invalid glob or unresolved template variable blocks Apply; unsaved edits survive navigation and are never mistaken for applied runtime state.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.103; plates pp.129–130; controls `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`.

## S12 — Memory, learned facts and retention

**Purpose.** Separate learned failure priors, user instructions and proposed personal/project memory; each record has scope, source, date and deletion semantics.

**Action.** Review a proposed memory diff before saving; open the originating evidence or delete the exact record after scope review.

**Recovery.** Deleting a memory affects future use after acknowledgement, not copies in prior calls; a project cannot redirect global memory storage.

**Controls.** `K-05 K-10 K-11 O-09 I-11`

**Layout regions.** Scope selector, memory records, provenance/evidence, retention/delete detail, learned-failure section.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Deleting a memory affects future use after acknowledgement, not copies in prior calls; a project cannot redirect global memory storage.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.103; plates pp.131–132; controls `K-05 K-10 K-11 O-09 I-11`.

## S13 — Permissions and sandbox policy editor

**Purpose.** Explain effective permissions by tool, action, target and scope; show persistent rules separately from pending approvals and actual containment.

**Action.** Preview a rule match against a sample operation, then apply a versioned policy to future dispatch after daemon acknowledgement.

**Recovery.** No policy store means no Remember rule; revocation reports future denial and already-issued effects separately; sandbox failure remains visible.

**Controls.** `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`

**Layout regions.** Permission preset, filesystem/shell/network sections, matcher/rule editor, effective-policy preview, audit/revocation.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: No policy store means no Remember rule; revocation reports future denial and already-issued effects separately; sandbox failure remains visible.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W08, W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.104; plates pp.133–134; controls `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`.

## S14 — Approval: exact operation and safe response

**Purpose.** Approval dock shows requesting session/child, operation, target, reason and effect boundary without losing the transcript; Allow once/Deny address one exact waiter generation.

**Action.** Allow once or Deny to one exact waiter generation; Remember opens a separate persistent-policy draft only when supported.

**Recovery.** Expired, already-replied or wrong-session requests disable response and offer Refresh; a timeout preserves unknown state until reconciled.

**Controls.** `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`

**Layout regions.** Requesting owner/child/generation, exact operation/arguments/cwd/target, effect boundary, Allow once/Deny, Remember draft, expiry.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Expired, already-replied or wrong-session requests disable response and offer Refresh; a timeout preserves unknown state until reconciled.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.104; plates pp.135–136; controls `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`.

## S15 — Tools and MCP connection health

**Purpose.** Group built-in tools and MCP servers by origin, availability and active permissions; discovered, enabled, healthy and authorized are independently visible.

**Action.** Open a server to inspect tool schemas, permission defaults, output bounds and logs; reconnect only through owned lifecycle control.

**Recovery.** Authentication required, failed discovery and stopped process are different states; unknown remote effects never become Cancelled merely on disconnect.

**Controls.** `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`

**Layout regions.** Built-in tools, MCP server list, tool catalog, permission/default badges, health/auth/logs, reconnect/remove.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Authentication required, failed discovery and stopped process are different states; unknown remote effects never become Cancelled merely on disconnect.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W08. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.104; plates pp.137–138; controls `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`.

## S16 — Add MCP server with trust and bounded config

**Purpose.** Configure a supervised stdio server with exact command/args and explicit trust; remote transport is offered only after capability negotiation; repository content never auto-starts a server.

**Action.** Validate size/count bounds, review executable host and source, then save and explicitly start the trusted server.

**Recovery.** A repository-supplied executable never starts merely because a config file exists; invalid args or a failed start preserve the previous valid server.

**Controls.** `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`

**Layout regions.** Transport selector, identity/command/args, environment references, trust/source review, bounds, test/start actions.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A repository-supplied executable never starts merely because a config file exists; invalid args or a failed start preserve the previous valid server.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W08. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.104; plates pp.139–140; controls `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`.

## S17 — Skills and command discovery

**Purpose.** Discover reusable workflows without confusing installation with permission grants; commands say whether they insert text, start a task or execute a bounded action.

**Action.** Open a skill manifest and version diff before enabling; insert a command into a draft without unexpectedly sending it.

**Recovery.** Unavailable dependencies or a failed update keep the previous known-good version; disabling a skill removes both discovery and agent invocation.

**Controls.** `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`

**Layout regions.** Skill library, manifest/version, source/trust, invocation policy, arguments/template preview, install/update/remove.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unavailable dependencies or a failed update keep the previous known-good version; disabling a skill removes both discovery and agent invocation.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.105; plates pp.141–142; controls `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`.

## S18 — Hooks: lifecycle automation and testing

**Purpose.** Edit exact native lifecycle events, executable arguments, environment, bounds and failure policy; test under the same capability envelope; partial output cannot authorize after timeout.

**Action.** Select an event, inspect matching inputs and run an explicitly admitted test; save only a validated version for future events.

**Recovery.** Deadline, nonzero exit and malformed verdict remain distinct; partial output cannot become an Allow verdict after timeout.

**Controls.** `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`

**Layout regions.** Native event list, hook spec form (executable/args/env/deadline/caps/failure), match preview, bounded test result, audit artifact.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Deadline, nonzero exit and malformed verdict remain distinct; partial output cannot become an Allow verdict after timeout.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W10. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.105; plates pp.143–144; controls `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`.

## S19 — Terminal ownership and background work

**Purpose.** An owned process table and bounded output coexist with the transcript; terminal kill, run stop and session abort are separate actions; stale PIDs cannot authorize kill.

**Action.** Select a live process, inspect cwd/sandbox/output, then use only controls supported by its current owned handle.

**Recovery.** A stale PID cannot authorize kill; timeout or disconnect retains Unknown effect until reconciliation, with no blind replay.

**Controls.** `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`

**Layout regions.** Owned process table, cwd/sandbox profile, bounded output, process/task/session controls, artifact links.

**VS Code variant.** Faktor editor tab for wide detail; terminal/output remains bounded and anchored.

**JetBrains variant.** Tool-window detail with selector/one visible surface; native editor/output tabs for long artifacts.

**Visual.** 12/16 px panel inset; 32 px control rows; bounded output with artifact link; 240 px stacks controls, never hides Stop/attention.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A stale PID cannot authorize kill; timeout or disconnect retains Unknown effect until reconciliation, with no blind replay.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W12. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.105; plates pp.145–146; controls `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`.

## S20 — Browser, diagnostics and editor actions

**Purpose.** Integrate host diagnostics and explicit code actions; browser controls require an actual advertised adapter with account, machine and network scope.

**Action.** Capture versioned selection or diagnostics into a draft; connect a browser only through an implemented adapter and scoped permission decision.

**Recovery.** A screenshot or clean console does not prove functional correctness; unavailable browser support is labelled rather than shown as an idle tab.

**Controls.** `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`

**Layout regions.** Browser adapter/connection, URL/account/machine scope, screenshot/console/network observations, diagnostics and editor actions.

**VS Code variant.** Faktor editor tab for wide detail; terminal/output remains bounded and anchored.

**JetBrains variant.** Tool-window detail with selector/one visible surface; native editor/output tabs for long artifacts.

**Visual.** 12/16 px panel inset; 32 px control rows; bounded output with artifact link; 240 px stacks controls, never hides Stop/attention.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A screenshot or clean console does not prove functional correctness; unavailable browser support is labelled rather than shown as an idle tab.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.105; plates pp.147–148; controls `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`.

## S21 — Budgets, retry policy and loop guards

**Purpose.** Show exact run limits and current accounting beside reliability controls; a soft warning is different from enforced admission refusal; a cap decrease cannot erase obligations.

**Action.** Stage a new default or request a supported active-task allowance revision; clearly label which captured task is affected.

**Recovery.** Unknown price is not zero; a cap decrease cannot erase settled or reserved spend; retry only after classifying admission and external-effect state.

**Controls.** `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`

**Layout regions.** Token cap, money cap, threshold/warning, retry policy, loop guards, current ledger/reservation summary.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unknown price is not zero; a cap decrease cannot erase settled or reserved spend; retry only after classifying admission and external-effect state.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W13. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.106; plates pp.149–150; controls `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`.

## S22 — Live run: tools, queue and attention

**Purpose.** A live timeline explains what is active, waiting, queued, failed or uncertain; critical attention remains visible when details are collapsed.

**Action.** Inspect a tool call, edit the owned next-message queue or stop the current run; a steer action states its delivery point.

**Recovery.** Lost stream marks the projection stale without turning the task idle; reconnect from durable cursor and reconcile before enabling duplicate effects.

**Controls.** `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`

**Layout regions.** Run header, live timeline, tool-call detail, prompt queue, attention/approval dock, composer/status.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Lost stream marks the projection stale without turning the task idle; reconnect from durable cursor and reconcile before enabling duplicate effects.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W10, W12, W13. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.106; plates pp.151–152; controls `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`.

## S23 — Agents: delegation, ownership and handoff

**Purpose.** Inspect parent and child work as a tree with bounded concurrency, shared spend and candidate ownership; child output is evidence, not automatic parent success.

**Action.** Select a child to view its admitted role/model/tools and use supported Steer, Model or Stop controls.

**Recovery.** Stale child controls are fenced by session and generation; child failure preserves artifacts and the parent remaining obligations.

**Controls.** `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`

**Layout regions.** Agent tree, child role/model/status, parent budget/ownership, child controls, typed handoff/result.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Stale child controls are fenced by session and generation; child failure preserves artifacts and the parent remaining obligations.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.106; plates pp.153–154; controls `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`.

## S24 — Native changes and review obligations

**Purpose.** Use each host native diff editor; keep base/candidate identity, acceptance criteria and remaining verification adjacent to code review; review marks do not integrate or publish.

**Action.** Open a file/hunk, attach a versioned comment or prepare a new bounded task; review marks alone do not integrate or publish.

**Recovery.** Manual owner edits trigger compare-and-swap refusal; a changed or partially selected candidate needs matching new evidence.

**Controls.** `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`

**Layout regions.** File tree, native diff, versioned comments, acceptance criteria, verification/proof status, review/finish actions.

**VS Code variant.** Native VS Code diff editor/tab; review marks never land a change.

**JetBrains variant.** Native IntelliJ diff editor; review marks never land a change.

**Visual.** Native diff editor chrome; review/details inset 12–24 px; version badges and 32 px actions; no overlap of verification status with code.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Manual owner edits trigger compare-and-swap refusal; a changed or partially selected candidate needs matching new evidence.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W14, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.106; plates pp.155–156; controls `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`.

## S25 — Checkpoints, restore and fork preview

**Purpose.** Show recorded checkpoint identity, content coverage and branching separately; preview exact files and conversation range before any supported restore.

**Action.** Choose a stored checkpoint, inspect coverage and compare content preconditions before requesting a separate restore transaction.

**Recovery.** Hash-only entries may not support redo/diff; unrelated edits or missing content refuse restore; shell/network/SCM effects are disclosed as uncovered.

**Controls.** `V-04 V-05 V-06 V-12 D-06 Q-09`

**Layout regions.** Checkpoint timeline, coverage/preconditions, diff preview, restore/fork scope, uncovered-effects warning.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Hash-only entries may not support redo/diff; unrelated edits or missing content refuse restore; shell/network/SCM effects are disclosed as uncovered.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W14. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.107; plates pp.157–158; controls `V-04 V-05 V-06 V-12 D-06 Q-09`.

## S26 — History, search and parallel workspaces

**Purpose.** History shows durable conversation identity, worktree, status and engineering outcome; label title search separately from transcript find and indexed recall.

**Action.** Switch deliberately while retaining per-session drafts, scroll and pending attention; open supported session actions with precise retention scope.

**Recovery.** Late callbacks cannot populate the wrong conversation; an unavailable history service is not an empty list; archived is not deleted.

**Controls.** `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`

**Layout regions.** Filters (title, status, worktree, outcome), paged session list, selected session summary, actions, parallel worktree tabs.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Late callbacks cannot populate the wrong conversation; an unavailable history service is not an empty list; archived is not deleted.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W12, W13, W14, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.107; plates pp.159–160; controls `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`.

## S27 — Usage, context and engineering outcomes

**Purpose.** Separate token accounting, provider/cache prices, quota, local estimates and verification-based engineering cost; explain every denominator.

**Action.** Filter by workspace, model, cohort and time basis, then inspect the underlying attempted runs and verified landed changes.

**Recovery.** No verified changes means VCSC unavailable, not zero; cancelled/failed attempts remain in the eligible spend numerator.

**Controls.** `B-04 M-13 K-12 O-06 V-09 D-07`

**Layout regions.** Period/cohort filters, token/cost categories, provider/cache/quota, engineering outcomes, VCSC and denominators.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: No verified changes means VCSC unavailable, not zero; cancelled/failed attempts remain in the eligible spend numerator.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W13, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.107; plates pp.161–162; controls `B-04 M-13 K-12 O-06 V-09 D-07`.

## S28 — Routing, efficiency and retrieval controls

**Purpose.** Expose current production efficiency flags and off-switches with honest measurement, fallback and apply timing; duplicate semantic_context/information_gain aliases fail validation.

**Action.** Preview a config diff, validate aliases and save through supported operator/native pathways; keep provenance for each effective value.

**Recovery.** Missing embedding support falls back to lexical/symbol retrieval; duplicate semantic_context/information_gain aliases fail validation.

**Controls.** `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`

**Layout regions.** Routing policy, efficiency flags (failure learning, CCR, typed handoff, semantic context, rework), retrieval strategy, measured effects.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Missing embedding support falls back to lexical/symbol retrieval; duplicate semantic_context/information_gain aliases fail validation.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.107; plates pp.163–164; controls `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`.

## S29 — Settings: scope, provenance and atomic apply

**Purpose.** Search settings across allowed scopes and inspect each effective value, source, lock and timing; distinguish staged, saved and runtime-applied state; apply one validated atomic patch.

**Action.** Apply a validated atomic patch against a known config revision, or Reset to applied values; Restore defaults edits the form until Apply.

**Recovery.** A revision conflict opens a compare/rebase view; machine executable paths never inherit from untrusted project settings.

**Controls.** `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`

**Layout regions.** Scope/search, category navigation, form rows with effective source/lock, staged changes, Reset/Apply/Restore defaults.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A revision conflict opens a compare/rebase view; machine executable paths never inherit from untrusted project settings.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.108; plates pp.165–166; controls `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`.

## S30 — Diagnostics, runtime trust and recovery

**Purpose.** Show readiness, version compatibility, runtime identity and bounded diagnostics with an actionable typed cause; checksum mismatch refuses start; checksum match is not signature verification.

**Action.** Review a redacted diagnostic preview before export; explicit Start is separate from opening the tool window.

**Recovery.** Checksum mismatch refuses start; an unrecorded exact path needs deliberate trust; a checksum match does not claim publisher-signature verification.

**Controls.** `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`

**Layout regions.** Readiness/version, runtime path/checksum/trust, protocol compatibility, bounded diagnostics, logs, start/export/recovery actions.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Checksum mismatch refuses start; an unrecorded exact path needs deliberate trust; a checksum match does not claim publisher-signature verification.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W10, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.108; plates pp.167–168; controls `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`.

## S31 — Remote workspaces and execution boundary

**Purpose.** Display editor, execution, model, vault and candidate locations together; remote workspaces must not blur local/hosted boundaries or path ownership.

**Action.** Bind a conversation to one workspace/worktree/daemon generation; revalidate after container, SSH or execution-host changes.

**Recovery.** A host change invalidates process and permission handles until rebound; preserve drafts without replaying a captured effectful request elsewhere.

**Controls.** `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`

**Layout regions.** Binding identity (workspace/worktree/daemon), editor/execution/model/vault/worker locations, data boundary, revalidation/rebind.

**VS Code variant.** Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.

**JetBrains variant.** `Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.

**Visual.** Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A host change invalidates process and permission handles until rebound; preserve drafts without replaying a captured effectful request elsewhere.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W02, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.108; plates pp.169–170; controls `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`.

## S32 — Task outcome and finish proof

**Purpose.** Finish with explicit engineering status: required acceptance, claim evidence, matching integration and requested finish; conversation completion alone proves none of them.

**Action.** Open each evidence receipt, inspect matching candidate identity and resolve any missing required fact before the requested finish can be fulfilled.

**Recovery.** Missing unit evidence, failed integration or absent SCM configuration remains a named blocker; a successful chat response cannot turn it green.

**Controls.** `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`

**Layout regions.** Engineering outcome, required criteria, claim evidence, candidate/proof identity, landing/integration, finish level/receipt.

**VS Code variant.** Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.

**JetBrains variant.** Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.

**Visual.** 12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.

**States.** Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Missing unit evidence, failed integration or absent SCM configuration remains a named blocker; a successful chat response cannot turn it green.

**Navigation.** Reached from the Faktor view/settings entry and the native command/keymap; flows: W06, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).

**Traceability.** Contract p.108; plates pp.171–172; controls `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`.

