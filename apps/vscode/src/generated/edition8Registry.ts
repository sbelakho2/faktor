// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: docs/specs/edition8/registry.json (Revision 8, 17 October 2026)
// Regenerate: node scripts/edition8/codegen.mjs --write
//
// The 200 control records are unchanged from Edition 5; the Rev8 delta is the
// 72-screen placement map and the per-screen host contract.

export interface Edition8Screen {
  readonly id: string;
  readonly title: string;
  readonly group: string;
  readonly job: string;
  readonly action: string;
  readonly primaryAction: string;
  readonly controls: readonly string[];
  readonly surface: 'sidebar' | 'settings' | 'modal' | 'quickpick' | 'native-settings' | 'diff' | 'terminal' | 'browser' | 'narrow';
  readonly designPage: number;
  readonly contractPage: number;
  readonly layoutRegions: string;
  readonly vscodeVariant: string;
  readonly jetbrainsVariant: string;
  readonly visual: string;
  readonly states: string;
  readonly navigation: string;
  readonly traceability: string;
}

export interface Edition8Control {
  readonly id: string;
  readonly category: string;
  readonly categoryCode: string;
  readonly name: string;
  readonly control: string;
  readonly hosts: readonly string[];
  readonly hostApplicability: string;
  readonly screens: readonly string[];
  readonly primaryScreen: string;
  readonly defaultState: string;
  readonly defaultValue: string;
  readonly scope: string;
  readonly applyTiming: string;
  readonly behavior: string;
  readonly states: Readonly<Record<string, string>>;
  readonly emptyState: string;
  readonly errorState: string;
  readonly disabledState: string;
  readonly loadingState: string;
  readonly staleState: string;
  readonly pendingState: string;
  readonly classification: string;
  readonly configKeys: readonly string[];
  readonly endpoints: readonly { readonly declared: string; readonly state: string; readonly actual: string; readonly line?: number; readonly lines?: readonly number[] }[];
  readonly contractGap: string;
  readonly currentStatus: string;
  readonly acceptanceIds: readonly string[];
  readonly specPage: number;
}

export interface Edition8Workflow {
  readonly id: string;
  readonly title: string;
  readonly screens: readonly string[];
  readonly controls: readonly string[];
  readonly cells: readonly string[];
  readonly completeWhen: string | null;
  readonly forbiddenFakeStates: string | null;
}

export type E8Screen = Edition8Screen;
export type E8Control = Edition8Control;
export type E8WorkflowContract = Edition8Workflow;

export const E8_COUNTS = {
  "screens": 72,
  "host_contracts": 144,
  "controls": 200,
  "control_categories": 15,
  "acceptance": 40,
  "workflows": 16,
  "workflow_cells_per_host": 5,
  "workflow_cells_total": 160,
  "platform_records": 6,
  "reference_vectors": 288,
  "base_host_renderings": 288,
  "high_contrast_renderings": 144,
  "maximal_alternate_renderings": 576,
  "fixture_classes": 576
} as const;

export const E8_WIDTHS = [
  240,
  320,
  420,
  480,
  720
] as const;
export const E8_TEXT_SCALE = [
  1,
  1.5,
  2
] as const;
export const E8_FIXTURE_CLASSES = [
  "first-read",
  "empty",
  "ready",
  "stale",
  "error",
  "pending",
  "acknowledged",
  "scope-switch"
] as const;
export const E8_THEMES = {
  "vscode": [
    "dark-modern",
    "light-modern",
    "high-contrast-dark",
    "high-contrast-light"
  ],
  "jetbrains": [
    "light",
    "dark",
    "high-contrast"
  ]
} as const;

export const E8_SCREENS: readonly Edition8Screen[] = [
  {
    "id": "S34",
    "title": "First open",
    "group": "Setup and start",
    "job": "Setup and start. Open a project, review prerequisites and start the service explicitly while preserving the draft.",
    "action": "Start Faktor",
    "primaryAction": "Start Faktor",
    "controls": [
      "P-01",
      "Q-11",
      "O-04",
      "O-05"
    ],
    "surface": "sidebar",
    "designPage": 22,
    "contractPage": 23,
    "layoutRegions": "Work header and tab row (Work / Inspect / History) · Setup panel status: “Set up Faktor / Faktor isn’t running” · Prerequisite checklist: service executable, model, shell isolation · Draft-retained notice · Sticky action row: Start Faktor, Setup Guide",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Service not running; prerequisites unselected/not connected/fail-closed; draft retained.",
    "navigation": "Entered from Work when the service/model/setup is absent; Back returns to Work with the draft intact; Setup Guide opens the setup documentation.",
    "traceability": "Design p.22; contract p.23; controls `O-04 O-05 P-01 Q-11`."
  },
  {
    "id": "S35",
    "title": "Service executable trust",
    "group": "Setup and start",
    "job": "Setup and start. Inspect the exact machine executable and its separate checksum and publisher trust basis before any start.",
    "action": "Confirm Path",
    "primaryAction": "Confirm Path",
    "controls": [
      "O-04",
      "O-05"
    ],
    "surface": "modal",
    "designPage": 25,
    "contractPage": 26,
    "layoutRegions": "Modal title and executable path · Bounded executable source preview · Source checksum row and publisher/signature basis · Trust confirmation checkbox (“I reviewed this executable path”) · Confirm Path / Show Details / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; executable preview; trust checkbox.",
    "states": "Checksum not recorded; signature not verified; trust is per exe path and machine; cancel keeps the previous trust state.",
    "navigation": "Opened from S34 “Locate…” or the runtime path setting; Confirm Path returns to S34; Cancel keeps the previous trust state.",
    "traceability": "Design p.25; contract p.26; controls `O-04 O-05`."
  },
  {
    "id": "S57",
    "title": "Shell isolation setup",
    "group": "Setup and start",
    "job": "Setup and start. Choose supported shell isolation explicitly and show platform-established filesystem and network guarantees.",
    "action": "Keep Fail-Closed · JetBrains: OK / Apply",
    "primaryAction": "Keep Fail-Closed · JetBrains: OK / Apply",
    "controls": [
      "Q-10",
      "Q-11"
    ],
    "surface": "settings",
    "designPage": 27,
    "contractPage": 28,
    "layoutRegions": "Machine-setting header and reset · Isolation choice: fail-closed (recommended) / network-capable · Reported filesystem and network capabilities · Open Platform Shell Setup action · Settings footer: OK / Cancel / Apply",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; isolation radio; platform capability rows.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from S34 “Review…” or Settings > Faktor > Shell Isolation; OK/Apply returns to the invoker; Platform Shell Setup opens host settings.",
    "traceability": "Design p.27; contract p.28; controls `Q-10 Q-11`."
  },
  {
    "id": "S36",
    "title": "Ready conversation",
    "group": "Setup and start",
    "job": "Setup and start. Write one task, choose an exact model and intent, and review the captured authority, criteria, finish and run caps.",
    "action": "Run Task (composer)",
    "primaryAction": "Run Task (composer)",
    "controls": [
      "M-02",
      "A-01",
      "Q-01",
      "B-01",
      "B-02",
      "V-07",
      "V-10"
    ],
    "surface": "sidebar",
    "designPage": 29,
    "contractPage": 30,
    "layoutRegions": "Work header · Goal composer with placeholder · Task-options summary (effects, finish, cap) · Context and model/effort row · Run Task primary action; Ctrl/Cmd+Enter",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar composer; one primary Run Task; one status line.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Work destination; Run Task admits the captured task; Task options and attachments open as sheets without losing the draft.",
    "traceability": "Design p.29; contract p.30; controls `A-01 M-02 Q-01 V-07 V-10 B-01 B-02`."
  },
  {
    "id": "S33",
    "title": "Task options",
    "group": "Setup and start",
    "job": "Setup and start. Edit the goal, required criteria, completion contract, finish level and exact run caps in one draft sheet. The active task keeps its",
    "action": "Use Options",
    "primaryAction": "Use Options",
    "controls": [
      "B-01",
      "B-02",
      "V-07",
      "V-10"
    ],
    "surface": "settings",
    "designPage": 31,
    "contractPage": 32,
    "layoutRegions": "Goal field · Acceptance-criteria rows and Add Criterion · Constraints, risks and claims summary · Finish ladder Local / Commit / Push / PR · Token cap and money cap with limit behavior · Use Options / Cancel",
    "vscodeVariant": "Dark Modern editor tab · webview.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Editor-tab sheet/dialog; criteria list; finish ladder; caps.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from the Work composer “Task options”; Use Options applies to the draft; Cancel keeps the prior captured options.",
    "traceability": "Design p.31; contract p.32; controls `V-07 V-10 B-01 B-02`."
  },
  {
    "id": "S37",
    "title": "Attachment admission",
    "group": "Setup and start",
    "job": "Setup and start. Resolve file, image and upload admission per source; failed or unsupported sources remain visible in the draft.",
    "action": "Use Attachments",
    "primaryAction": "Use Attachments",
    "controls": [
      "C-03",
      "C-05",
      "C-06"
    ],
    "surface": "sidebar",
    "designPage": 33,
    "contractPage": 34,
    "layoutRegions": "Attachment list with per-source status · Selected source details (type, size, range, version) · Unsupported/failed rows with Remove/Retry · Add Files action · Use Attachments",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; attachment rows; failed/unsupported rows stay visible.",
    "states": "2 need attention; unsupported image for the chosen model; interrupted upload; retry/remove; failed sources stay visible.",
    "navigation": "Opened from the Work composer attachment control; Use Attachments returns to the draft; failed sources remain until removed/retried.",
    "traceability": "Design p.33; contract p.34; controls `C-03 C-05 C-06`."
  },
  {
    "id": "S38",
    "title": "Task admission pending",
    "group": "Setup and start",
    "job": "Setup and start. Show one captured logical submission while the admission receipt is pending; suppress duplicate starts.",
    "action": "Queue Message (composer)",
    "primaryAction": "Queue Message (composer)",
    "controls": [
      "V-07",
      "D-05"
    ],
    "surface": "sidebar",
    "designPage": 35,
    "contractPage": 36,
    "layoutRegions": "Sent user message · “Starting…” admission status · Running configuration summary (Code, effects, finish) · Composer with next-message note · Context/model/usage row",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; starting state; composer stays available.",
    "states": "Starting…; duplicate starts suppressed; next message does not change the submitted start.",
    "navigation": "Work destination immediately after Run Task; starting state resolves to admitted or S39; the composer stays available for the next queued message.",
    "traceability": "Design p.35; contract p.36; controls `D-05 V-07`."
  },
  {
    "id": "S39",
    "title": "Ambiguous task start",
    "group": "Setup and start",
    "job": "Setup and start. Reconcile an ambiguous admission against the original request identity and body before any permitted retry.",
    "action": "Check Task Status",
    "primaryAction": "Check Task Status",
    "controls": [
      "B-12",
      "D-05"
    ],
    "surface": "sidebar",
    "designPage": 37,
    "contractPage": 38,
    "layoutRegions": "Start-not-confirmed banner · Connection-dropped explanation · Kept draft summary: task/criteria, attachments/options · Retry-safety note (same request cannot start twice) · Check Task Status / Keep Draft",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; banner + kept-draft summary.",
    "states": "Start not confirmed; connection dropped after sending; task/criteria and attachments/options kept; retry sends the same request identity.",
    "navigation": "Opened from S38 when admission is ambiguous; Check Task Status reconciles the original request; Keep Draft returns to Work.",
    "traceability": "Design p.37; contract p.38; controls `B-12 D-05`."
  },
  {
    "id": "S01",
    "title": "Work",
    "group": "Work",
    "job": "Work. The compact tool window exposes intent, model, effort, authority and context beside the draft. Running work keeps the active",
    "action": "Queue Message (composer)",
    "primaryAction": "Queue Message (composer)",
    "controls": [
      "M-01",
      "M-02",
      "M-04",
      "G-01",
      "A-01",
      "C-01",
      "Q-01",
      "B-01",
      "B-02",
      "V-07",
      "V-10",
      "D-05",
      "D-07"
    ],
    "surface": "sidebar",
    "designPage": 40,
    "contractPage": 41,
    "layoutRegions": "Task header: title, status, Stop · Pinned attention/approval card · Transcript (user/Faktor turns) · Queue composer · Configuration and context-usage row",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar cockpit; pinned attention; transcript; composer; quiet config row.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Work destination; attention card opens S14; queue opens S59; Stop opens S70; Inspect/History switch destinations.",
    "traceability": "Design p.40; contract p.41; controls `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`."
  },
  {
    "id": "S14",
    "title": "Approve an action",
    "group": "Work",
    "job": "Work. The approval dock shows the requesting session/child, operation, target, reason and effect boundary without losing the running",
    "action": "Allow Once",
    "primaryAction": "Allow Once",
    "controls": [
      "Q-06",
      "Q-07",
      "Q-09",
      "Q-13",
      "X-12",
      "B-12",
      "D-12"
    ],
    "surface": "sidebar",
    "designPage": 42,
    "contractPage": 43,
    "layoutRegions": "Approval header and requester · Exact command and target · Effect boundary: network, writable paths, requested scope · Why / AC reference · Request details disclosure · Create a Rule for Future Requests / Allow Once / Deny",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar attention/dock; exact command in monospace; one primary Allow Once.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from the pinned attention card or host notification; Allow Once/Deny resolves the exact waiter; Details expands in place.",
    "traceability": "Design p.42; contract p.43; controls `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`."
  },
  {
    "id": "S40",
    "title": "Answer a task question",
    "group": "Work",
    "job": "Work. Answer the exact current task question and record the response without changing the original task contract.",
    "action": "Send Answer",
    "primaryAction": "Send Answer",
    "controls": [
      "V-07",
      "D-07"
    ],
    "surface": "sidebar",
    "designPage": 44,
    "contractPage": 45,
    "layoutRegions": "Question header and waiting status · Question text and context · Answer choices and optional constraint · Send Answer / Review Task",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar question card; choices plus optional constraint.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from a question attention row; Send Answer returns to Work; Review Task opens the task contract.",
    "traceability": "Design p.44; contract p.45; controls `D-07 V-07`."
  },
  {
    "id": "S22",
    "title": "Live activity",
    "group": "Work",
    "job": "Work. A live timeline explains what is active, waiting, queued, failed or uncertain. Critical attention remains visible when details are",
    "action": "Open Approval",
    "primaryAction": "Open Approval",
    "controls": [
      "M-11",
      "G-14",
      "Q-06",
      "T-09",
      "B-04",
      "D-05",
      "D-07",
      "D-08",
      "D-12",
      "D-13"
    ],
    "surface": "sidebar",
    "designPage": 46,
    "contractPage": 47,
    "layoutRegions": "Activity timeline with status dots · Queued-next row · Open Approval / Stop Task actions · Collapsed critical attention",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Overview > Activity from Work/Inspect; Open Approval opens S14; Stop Task uses S70 scope.",
    "traceability": "Design p.46; contract p.47; controls `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`."
  },
  {
    "id": "S59",
    "title": "Queued messages",
    "group": "Work",
    "job": "Work. Edit, reorder or remove only unadmitted queued inputs through an acknowledged queue revision.",
    "action": "Save Edit",
    "primaryAction": "Save Edit",
    "controls": [
      "D-05"
    ],
    "surface": "sidebar",
    "designPage": 49,
    "contractPage": 50,
    "layoutRegions": "Queued-message list with order/status · Move Up/Move Down · Edit-message field · Save Edit / Remove",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar queue list; one edit field.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from the Work queue control; Save Edit applies to an unadmitted item; Move Up/Down reorders; back returns to Work.",
    "traceability": "Design p.49; contract p.50; controls `D-05`."
  },
  {
    "id": "S70",
    "title": "Scoped stop actions",
    "group": "Work",
    "job": "Work. Choose run cancellation, turn abort or owned-process stop and acknowledge the requested operation separately.",
    "action": "Stop Run",
    "primaryAction": "Stop Run",
    "controls": [
      "A-10",
      "X-05",
      "D-07"
    ],
    "surface": "modal",
    "designPage": 51,
    "contractPage": 52,
    "layoutRegions": "Stop-scope dialog: run / current response / process · Consequence copy for each scope · More Options for process scope · Stop Run / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; three stop scopes; consequence copy.",
    "states": "Stop run/current response/process are separate; already-issued commands/network calls cannot be undone.",
    "navigation": "Opened from Stop in the task header or Stop › More Options; Stop Run/Cancel returns to Work; process scope remains separate.",
    "traceability": "Design p.51; contract p.52; controls `A-10 X-05 D-07`."
  },
  {
    "id": "S41",
    "title": "Inspect overview",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Read all four engineering boundaries and open the selected plan, criterion, artifact or candidate from one overview.",
    "action": "Review Missing Evidence",
    "primaryAction": "Review Missing Evidence",
    "controls": [
      "V-07",
      "V-08",
      "V-09",
      "V-10"
    ],
    "surface": "sidebar",
    "designPage": 55,
    "contractPage": 56,
    "layoutRegions": "Completion summary: criteria, claims, integration, finish · Explore: plan, changed files, activity · Review Missing Evidence action",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar overview; four completion rows; explore rows.",
    "states": "Not complete; criterion pending; claim needs evidence; integration not started; finish pending.",
    "navigation": "Inspect destination overview; rows open S42, S24, S22; Review Missing Evidence opens S45.",
    "traceability": "Design p.55; contract p.56; controls `V-07 V-08 V-09 V-10`."
  },
  {
    "id": "S42",
    "title": "Plan and dependencies",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Inspect the dependency-ordered plan and selected step without confusing a planned checkmark with observed proof.",
    "action": "Open AC-03",
    "primaryAction": "Open AC-03",
    "controls": [
      "V-07",
      "V-08"
    ],
    "surface": "sidebar",
    "designPage": 57,
    "contractPage": 58,
    "layoutRegions": "Dependency-ordered step list with statuses · Selected step detail and evidence requirement · Constraints/invariants · Open AC-03",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; step list; selected detail.",
    "states": "Done/waiting/blocked step states; planned checkmarks are not observed proof.",
    "navigation": "Overview > Plan; Open AC-03 opens S45/S46; Back returns to the exact plan row.",
    "traceability": "Design p.57; contract p.58; controls `V-07 V-08`."
  },
  {
    "id": "S24",
    "title": "Review changes",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Use each host’s native diff editor. Keep base/candidate identity, acceptance criteria and remaining verification adjacent to code",
    "action": "Open Diff",
    "primaryAction": "Open Diff",
    "controls": [
      "A-11",
      "V-01",
      "V-02",
      "V-03",
      "V-07",
      "V-09",
      "V-11",
      "V-12"
    ],
    "surface": "sidebar",
    "designPage": 59,
    "contractPage": 60,
    "layoutRegions": "Changed-file list with hunk stats and comments · Native diff preview · Verification/evidence prompt · Open Diff / Add Review Task",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Overview > Changes; Open Diff opens S43; Add Review Task creates a bounded review child; back returns to Overview.",
    "traceability": "Design p.59; contract p.60; controls `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`."
  },
  {
    "id": "S43",
    "title": "Native diff review",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Review the immutable base and selected candidate in the native editor with exact hunk and claim navigation.",
    "action": "Open Related Criterion",
    "primaryAction": "Open Related Criterion",
    "controls": [
      "V-01",
      "V-03"
    ],
    "surface": "diff",
    "designPage": 61,
    "contractPage": 62,
    "layoutRegions": "Native base/candidate diff editor · Change counter and hunk navigation · Open Related Criterion · Read-only base/candidate labels",
    "vscodeVariant": "Dark Modern diff editor · VS Code diff.",
    "jetbrainsVariant": "Light diff viewer · DiffManager.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native diff editor: base read-only, candidate read-only, hunk navigation, no custom chrome.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Native diff editor opened from S24; Open Related Criterion returns to S45; closing restores the previous editor/selection.",
    "traceability": "Design p.61; contract p.62; controls `V-01 V-03`."
  },
  {
    "id": "S45",
    "title": "Required verification",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Inspect every required criterion, its current evidence and explicit pending, failed, refused or unavailable result.",
    "action": "Open AC-03 Details",
    "primaryAction": "Open AC-03 Details",
    "controls": [
      "V-07",
      "V-08",
      "V-09"
    ],
    "surface": "sidebar",
    "designPage": 63,
    "contractPage": 64,
    "layoutRegions": "Required-criteria list with result and evidence · Selected criterion detail: pending/failed/refused/unavailable · Verification basis: requirement, claim, source, candidate · Open AC-03 Details / Back to Overview",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; criteria list; selected evidence detail.",
    "states": "Passed / Needs evidence / Failed / Refused / Unavailable; pending stays pending.",
    "navigation": "Overview > Verification or S41/S42; Open AC-03 Details opens S46; Back to Overview restores selection.",
    "traceability": "Design p.63; contract p.64; controls `V-07 V-08 V-09`."
  },
  {
    "id": "S46",
    "title": "Evidence detail",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Follow one criterion to its exact artifact, source, claim and candidate; open bounded excerpts and full content.",
    "action": "Open Full Output",
    "primaryAction": "Open Full Output",
    "controls": [
      "V-07",
      "V-08"
    ],
    "surface": "sidebar",
    "designPage": 65,
    "contractPage": 66,
    "layoutRegions": "Evidence header and result · Bounded excerpt · Source path/range · Claim and candidate identity · Open Full Output",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; excerpt; source/claim/candidate.",
    "states": "Passed evidence with bounded excerpt; source/claim/candidate identity.",
    "navigation": "Opened from S45; Open Full Output opens the bounded artifact view; back returns to the criterion.",
    "traceability": "Design p.65; contract p.66; controls `V-07 V-08`."
  },
  {
    "id": "S47",
    "title": "Proof identity mismatch",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Remove stale verified presentation when proof belongs to a different candidate; retain the historical record separately.",
    "action": "Verify Current Candidate",
    "primaryAction": "Verify Current Candidate",
    "controls": [
      "V-09",
      "V-12"
    ],
    "surface": "sidebar",
    "designPage": 67,
    "contractPage": 68,
    "layoutRegions": "Out-of-date proof banner · Verified vs current candidate identities · Criteria/claims/integration status · Full proof record disclosure · Verify Current Candidate / Open Previous Results",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; stale proof banner; identity pairs.",
    "states": "Verification out of date; verified c-07 vs current c-08; historical record remains separate.",
    "navigation": "Overview > Proof when candidate identity changes; Verify Current Candidate re-runs required checks; Open Previous Results keeps history separate.",
    "traceability": "Design p.67; contract p.68; controls `V-09 V-12`."
  },
  {
    "id": "S44",
    "title": "Integration conflict",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Preserve user edits when landing preconditions change; review a new candidate and rerun affected verification.",
    "action": "Review Conflict",
    "primaryAction": "Review Conflict",
    "controls": [
      "V-01",
      "V-12"
    ],
    "surface": "sidebar",
    "designPage": 69,
    "contractPage": 70,
    "layoutRegions": "Workspace-changed banner · Candidate vs workspace columns · Per-file conflict/waiting rows · Review Conflict / Keep Candidate",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; candidate/workspace columns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Overview > Integration when landing preconditions change; Review Conflict opens the native diff/merge; Keep Candidate abandons the new landing.",
    "traceability": "Design p.69; contract p.70; controls `V-01 V-12`."
  },
  {
    "id": "S48",
    "title": "Finish actions",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Inspect the hierarchical Local / Commit / Push / PR receipts and resolve only the blocked native finish step.",
    "action": "Configure Remote…",
    "primaryAction": "Configure Remote…",
    "controls": [
      "V-10",
      "V-11"
    ],
    "surface": "sidebar",
    "designPage": 71,
    "contractPage": 72,
    "layoutRegions": "Finish header and level · Hierarchical Local / Commit / Push / Pull request rows · Per-step receipt/status and blocked cause · Configure Remote… / Keep Result",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; finish ladder; receipts.",
    "states": "Done / Blocked / Not started; blocked push names Configure Remote…; completed steps do not rerun.",
    "navigation": "Overview > Finish from S32/S65; Configure Remote… opens host SCM configuration; Keep Result returns to the outcome.",
    "traceability": "Design p.71; contract p.72; controls `V-10 V-11`."
  },
  {
    "id": "S32",
    "title": "Task outcome",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Finish with explicit engineering status: required acceptance, claim evidence, matching integration and requested finish.",
    "action": "Open Required Verification",
    "primaryAction": "Open Required Verification",
    "controls": [
      "B-04",
      "V-07",
      "V-08",
      "V-09",
      "V-10",
      "V-11",
      "V-12",
      "O-06"
    ],
    "surface": "sidebar",
    "designPage": 73,
    "contractPage": 74,
    "layoutRegions": "Outcome status banner · Four-row boundary summary: acceptance, claims, integration, finish · Actions: Open Required Verification, Inspect Artifacts",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; four-boundary outcome.",
    "states": "Not complete yet; needs evidence; acceptance/claims/integration/finish rows pending.",
    "navigation": "Overview > Outcome from Work/Inspect; Open Required Verification opens S45; Inspect Artifacts opens evidence.",
    "traceability": "Design p.73; contract p.74; controls `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`."
  },
  {
    "id": "S65",
    "title": "Verified outcome",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Show a separate complete example only when all required acceptance, claims, matching integration and finish agree.",
    "action": "Review Integrated Change",
    "primaryAction": "Review Integrated Change",
    "controls": [
      "V-09",
      "V-10"
    ],
    "surface": "sidebar",
    "designPage": 75,
    "contractPage": 76,
    "layoutRegions": "Verified-complete banner · Four-row all-pass summary · Full proof record · Review Integrated Change / New Task",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; verified outcome; proof record.",
    "states": "Verified complete: all four boundaries pass; proof record remains inspectable.",
    "navigation": "Overview > Outcome when all four boundaries pass; Review Integrated Change opens S43; New Task starts a fresh conversation.",
    "traceability": "Design p.75; contract p.76; controls `V-09 V-10`."
  },
  {
    "id": "S25",
    "title": "Restore & fork",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Show recorded checkpoint identity, content coverage and branching separately. Preview the exact files and conversation range",
    "action": "Review Restore…",
    "primaryAction": "Review Restore…",
    "controls": [
      "Q-09",
      "V-04",
      "V-05",
      "V-06",
      "V-12",
      "D-06"
    ],
    "surface": "sidebar",
    "designPage": 77,
    "contractPage": 78,
    "layoutRegions": "Checkpoint list with coverage · Restore-scope segmented control: Code / Conversation / Both · Affected-file list with readiness · Uncovered-effects warning · Review Restore… / Fork Conversation",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; checkpoint list; restore scope.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "History/Overview > Checkpoints; Review Restore… previews files; Fork Conversation creates separate lineage; back restores selection.",
    "traceability": "Design p.77; contract p.78; controls `V-04 V-05 V-06 V-12 D-06 Q-09`."
  },
  {
    "id": "S49",
    "title": "Cancelled run",
    "group": "Inspect and proof",
    "job": "Inspect and proof. Retain candidate work, evidence, recorded spend and drafts after a durable cancellation; completion remains separate.",
    "action": "Review Kept Work",
    "primaryAction": "Review Kept Work",
    "controls": [
      "B-04",
      "V-04",
      "D-07"
    ],
    "surface": "sidebar",
    "designPage": 79,
    "contractPage": 80,
    "layoutRegions": "Run-cancelled status · Kept-work summary: candidate, verification, processes, spend, outcome · Review Kept Work / New Task",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; kept-work summary.",
    "states": "Cancelled/stopped; candidate, verification, processes, spend and outcome kept; completion remains separate.",
    "navigation": "Work/Inspect after a durable cancellation; Review Kept Work opens the candidate/diff/evidence; New Task leaves the kept work behind.",
    "traceability": "Design p.79; contract p.80; controls `D-07 V-04 B-04`."
  },
  {
    "id": "S23",
    "title": "Delegated agents",
    "group": "Agents",
    "job": "Agents. Inspect parent and child work as a tree with bounded concurrency, shared spend and candidate ownership. Child output is",
    "action": "Send Steer",
    "primaryAction": "Send Steer",
    "controls": [
      "M-11",
      "A-06",
      "A-07",
      "A-08",
      "A-09",
      "A-10",
      "A-11",
      "A-12",
      "Q-06",
      "B-04"
    ],
    "surface": "sidebar",
    "designPage": 82,
    "contractPage": 83,
    "layoutRegions": "Agents tree with role/status · Shared budget and concurrency summary · Steer composer · Agent details disclosure · Send Steer / Stop Verifier",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; agent tree; shared budget.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Inspect > Agents; selecting a child opens S50; approvals open S14; Compare opens S51; Coordination opens S52.",
    "traceability": "Design p.82; contract p.83; controls `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`."
  },
  {
    "id": "S50",
    "title": "Agent detail and steer",
    "group": "Agents",
    "job": "Agents. Inspect a child assignment, ownership, inherited authority and shared budget before steering or stopping that child.",
    "action": "Send Steer",
    "primaryAction": "Send Steer",
    "controls": [
      "A-09",
      "A-10",
      "A-11",
      "A-12"
    ],
    "surface": "sidebar",
    "designPage": 84,
    "contractPage": 85,
    "layoutRegions": "Child identity and status · Task/workspace ownership · Allowed capabilities and inherited authority · Budget reservation · Steer field with character count · Send Steer / Stop Verifier",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; child detail; steer field.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Agents > child; Send Steer records guidance without changing task/permissions; Stop Verifier uses child-scoped stop; back returns to the tree.",
    "traceability": "Design p.84; contract p.85; controls `A-09 A-10 A-11 A-12`."
  },
  {
    "id": "S51",
    "title": "Compare approaches",
    "group": "Agents",
    "job": "Agents. Compare candidate approaches against one contract and recommend without automatically integrating the winner.",
    "action": "Review Approach B",
    "primaryAction": "Review Approach B",
    "controls": [
      "M-14",
      "A-12",
      "V-07"
    ],
    "surface": "sidebar",
    "designPage": 86,
    "contractPage": 87,
    "layoutRegions": "Comparison cards A/B with criteria/spend · Recommendation and rationale · Comparison basis · Review Approach B / Stop Comparison",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; comparison cards.",
    "states": "Approach A pending (2/3); approach B recommended (3/3); choosing does not integrate.",
    "navigation": "Inspect > Agents > Compare; Review Approach B inspects the chosen candidate without integrating; Stop Comparison cancels both candidates.",
    "traceability": "Design p.86; contract p.87; controls `M-14 A-12 V-07`."
  },
  {
    "id": "S52",
    "title": "Coordination",
    "group": "Agents",
    "job": "Agents. Read and post bounded durable coordination notes with explicit read acknowledgement and stable revision paging.",
    "action": "Post Note",
    "primaryAction": "Post Note",
    "controls": [
      "A-12",
      "D-07"
    ],
    "surface": "sidebar",
    "designPage": 88,
    "contractPage": 89,
    "layoutRegions": "Notes list with unread/author/time · New-note subject/message · Post Note / Mark All Read",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar; durable notes.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Inspect > Agents > Coordination; Post Note appends to the durable board; Mark All Read acknowledges the loaded head; back returns to Agents.",
    "traceability": "Design p.88; contract p.89; controls `A-12 D-07`."
  },
  {
    "id": "S26",
    "title": "Conversation history",
    "group": "History",
    "job": "History. History shows durable conversation identity, worktree, status and engineering outcome. Label title search separately from",
    "action": "Open Conversation",
    "primaryAction": "Open Conversation",
    "controls": [
      "V-09",
      "D-01",
      "D-02",
      "D-03",
      "D-04",
      "D-06",
      "D-13",
      "D-15"
    ],
    "surface": "sidebar",
    "designPage": 91,
    "contractPage": 92,
    "layoutRegions": "Title search field · Conversation rows with status and worktree · Context menu: find, rename, pin, archive, fork, export, delete · Open Conversation / New Conversation",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar history list; title search.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "History destination; row opens the conversation; context menu actions stay in History; New Conversation returns to Work.",
    "traceability": "Design p.91; contract p.92; controls `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`."
  },
  {
    "id": "S61",
    "title": "Empty conversation history",
    "group": "History",
    "job": "History. Distinguish true empty history, no search matches and failed reads; lead naturally back to a retained draft.",
    "action": "Start a Task",
    "primaryAction": "Start a Task",
    "controls": [
      "D-01",
      "D-02"
    ],
    "surface": "sidebar",
    "designPage": 93,
    "contractPage": 94,
    "layoutRegions": "Empty-history message · Draft-retained row · Start a Task / Open Draft",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar empty state; draft retention.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "History empty state; Start a Task returns to Work; Open Draft restores the retained unsent draft.",
    "traceability": "Design p.93; contract p.94; controls `D-01 D-02`."
  },
  {
    "id": "S71",
    "title": "Find in conversation",
    "group": "History",
    "job": "History. Search the full durable conversation through the service cursor and return to the exact original message.",
    "action": "Go to Message",
    "primaryAction": "Go to Message",
    "controls": [
      "D-03"
    ],
    "surface": "sidebar",
    "designPage": 96,
    "contractPage": 97,
    "layoutRegions": "Conversation search field and result count · Result rows with role/snippet · Go to Message / Back to Conversation",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar search results; result count.",
    "states": "Result 1 of 12; search covers unloaded messages; return to exact original message.",
    "navigation": "Opened from History/Conversation Find or Ctrl/Cmd+F; Go to Message returns to the exact original message; Back returns to the conversation.",
    "traceability": "Design p.96; contract p.97; controls `D-03`."
  },
  {
    "id": "S72",
    "title": "Switch conversation safely",
    "group": "History",
    "job": "History. Preserve draft, caret, scroll, sources and owned work while rebinding every response and action to another conversation.",
    "action": "Switch",
    "primaryAction": "Switch",
    "controls": [
      "X-13",
      "D-01",
      "D-15"
    ],
    "surface": "modal",
    "designPage": 98,
    "contractPage": 99,
    "layoutRegions": "Switch-conversation confirmation · Target conversation identity · Draft/run retention facts · Switch / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; draft/run retention facts.",
    "states": "Draft/attachments/caret/scroll saved; run 047 keeps running in the other conversation.",
    "navigation": "Opened from the conversation switcher; Switch rebinds the view; Cancel keeps the current conversation; the other run keeps running.",
    "traceability": "Design p.98; contract p.99; controls `D-01 D-15 X-13`."
  },
  {
    "id": "S53",
    "title": "Recorded outcome cohort",
    "group": "Usage",
    "job": "Usage. Inspect an outcome cohort with eligible attempted spend, verified landed denominator and actual local/hosted routes.",
    "action": "Open Runs",
    "primaryAction": "Open Runs",
    "controls": [
      "M-13",
      "K-12",
      "O-06"
    ],
    "surface": "sidebar",
    "designPage": 101,
    "contractPage": 102,
    "layoutRegions": "Cohort header and time window · Cost per verified change · Failed/cancelled attempts and spend · Local vs hosted routes · Calculation method disclosure · Open Runs / Export…",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage cohort; denominator explanation.",
    "states": "Unavailable when no verified changes (never $0); failed/cancelled attempts included; local/hosted shown separately.",
    "navigation": "Usage > Outcomes; Open Runs opens the recorded run cohort; Export… exports the bounded cohort.",
    "traceability": "Design p.101; contract p.102; controls `O-06 K-12 M-13`."
  },
  {
    "id": "S54",
    "title": "Usage and quota",
    "group": "Usage",
    "job": "Usage. Separate provider API spend, optional subscription quota, reservations and unmeasured local operating cost.",
    "action": "None (read-only)",
    "primaryAction": "None (read-only)",
    "controls": [
      "O-06",
      "O-07"
    ],
    "surface": "sidebar",
    "designPage": 103,
    "contractPage": 104,
    "layoutRegions": "Usage categories: local inference, provider API, subscription quota, GPU cost · No-account/unavailable labels · Account Settings…",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage categories; read-only.",
    "states": "No account needed for local work; subscription quota and GPU cost unavailable, not zero.",
    "navigation": "Usage > Account; read-only; Account Settings… opens the optional control-plane account screen (S55).",
    "traceability": "Design p.103; contract p.104; controls `O-06 O-07`."
  },
  {
    "id": "S27",
    "title": "Usage & outcomes",
    "group": "Usage",
    "job": "Usage. Separate token accounting, provider/cache prices, quota, local estimates and verification-based engineering cost. Explain every",
    "action": "Inspect Runs",
    "primaryAction": "Inspect Runs",
    "controls": [
      "M-13",
      "K-12",
      "B-04",
      "V-09",
      "D-07",
      "O-06"
    ],
    "surface": "sidebar",
    "designPage": 105,
    "contractPage": 106,
    "layoutRegions": "Usage summary and cost per verified change · Token/cache breakdown · Provider quota · How costs are calculated · Inspect Runs / Export…",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar usage summary; token/cost breakdown.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Usage destination; Inspect Runs opens S53; Export… exports the bounded usage view.",
    "traceability": "Design p.105; contract p.106; controls `B-04 M-13 K-12 O-06 V-09 D-07`."
  },
  {
    "id": "S02",
    "title": "Choose a model",
    "group": "Models and providers",
    "job": "Models and providers. Search by exact ID, provider or capability. A selected item exposes authority, limits and availability before selection; favorite and",
    "action": "Enter selects",
    "primaryAction": "Enter selects",
    "controls": [
      "M-01",
      "M-02",
      "M-03",
      "M-04",
      "M-05",
      "M-06",
      "M-07",
      "M-08",
      "M-09",
      "M-10",
      "M-11",
      "M-12",
      "M-13",
      "M-14"
    ],
    "surface": "quickpick",
    "designPage": 108,
    "contractPage": 109,
    "layoutRegions": "Search field · Current model group · Favorites/recent groups · Served-model list with capability/authority · Unavailable model rows with reason · Selected-model detail; apply timing",
    "vscodeVariant": "Dark Modern Quick Pick · native.",
    "jetbrainsVariant": "Light popup chooser · native.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser; 32 px rows; Enter/Esc/Tab host focus.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from the Work model control or Settings > Faktor > Models; Enter selects, Escape cancels and restores the previous pair; capability details expand in place.",
    "traceability": "Design p.108; contract p.109; controls `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`."
  },
  {
    "id": "S03",
    "title": "Provider connection",
    "group": "Models and providers",
    "job": "Models and providers. Connections have a stable instance ID, API family, endpoint and credential reference. Test auth, model discovery and capability",
    "action": "Save Connection · JetBrains: OK / Apply",
    "primaryAction": "Save Connection · JetBrains: OK / Apply",
    "controls": [
      "P-01",
      "P-02",
      "P-03",
      "P-04",
      "P-05",
      "P-06",
      "P-10",
      "P-11",
      "P-12",
      "P-14"
    ],
    "surface": "settings",
    "designPage": 110,
    "contractPage": 111,
    "layoutRegions": "Connection name and instance ID · API family picker · Base URL and resolved host · Credential reference and custom headers · Connection test/health status · Save Connection / Test Connection",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; label/value rows; Test Connection stays adjacent.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Providers > connection; Save Connection applies one validated patch; Test Connection probes without saving; OK/Apply returns to Settings.",
    "traceability": "Design p.110; contract p.111; controls `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`."
  },
  {
    "id": "S04",
    "title": "Local inference",
    "group": "Models and providers",
    "job": "Models and providers. Locality is relative to the daemon. Explain loopback, allocation, model readiness and resource ownership before offering local-",
    "action": "Save · JetBrains: OK / Apply",
    "primaryAction": "Save · JetBrains: OK / Apply",
    "controls": [
      "M-09",
      "P-07",
      "P-08",
      "P-09",
      "P-13",
      "P-15",
      "C-10"
    ],
    "surface": "settings",
    "designPage": 112,
    "contractPage": 113,
    "layoutRegions": "Adapter and endpoint · Locality explanation · Context allocation / keep-loaded policy · Loopback and private-network controls · Embedding model and retrieval fallback · Probe Model / Save",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; label/value rows; locality and allocation shown before runtime controls.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Providers > local connection; Save applies; Probe Model observes without certifying coding quality; OK/Apply returns to Settings.",
    "traceability": "Design p.112; contract p.113; controls `P-07 P-08 P-09 P-13 P-15 C-10 M-09`."
  },
  {
    "id": "S05",
    "title": "Model tuning",
    "group": "Models and providers",
    "job": "Models and providers. A capability-driven form exposes standard generation controls without pretending every provider supports every field. Unset is",
    "action": "Apply Tuning · JetBrains: OK / Apply",
    "primaryAction": "Apply Tuning · JetBrains: OK / Apply",
    "controls": [
      "G-01",
      "G-02",
      "G-03",
      "G-04",
      "G-05",
      "G-06",
      "G-07",
      "G-08",
      "G-09",
      "G-10",
      "G-11",
      "G-12",
      "G-13",
      "G-14"
    ],
    "surface": "settings",
    "designPage": 114,
    "contractPage": 115,
    "layoutRegions": "Model identity and apply timing · Reasoning effort · Maximum output · Temperature / top-p · Advanced parameters disclosure · Effective-request preview · Apply Tuning / Reset",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; reasoning segmented control; advanced parameters behind disclosure.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Model Tuning or the Work model detail; Apply Tuning applies to the next call; Reset removes overrides; Cancel preserves staged edits.",
    "traceability": "Design p.114; contract p.115; controls `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`."
  },
  {
    "id": "S63",
    "title": "Compare exact models",
    "group": "Models and providers",
    "job": "Models and providers. Compare exact provider/model pairs with held-constant task and context, separate candidates and shared spend.",
    "action": "Start Comparison",
    "primaryAction": "Start Comparison",
    "controls": [
      "M-14",
      "B-01",
      "B-02"
    ],
    "surface": "sidebar",
    "designPage": 116,
    "contractPage": 117,
    "layoutRegions": "Two exact model candidates with data boundary · Shared cap · Comparison scope · Start Comparison / Cancel",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from Work/Inspect Compare models; Start Comparison admits a bounded comparison; Cancel leaves the draft unchanged.",
    "traceability": "Design p.116; contract p.117; controls `M-14 B-01 B-02`."
  },
  {
    "id": "S06",
    "title": "Choose an agent",
    "group": "Models and providers",
    "job": "Models and providers. Code, Ask, Plan, Debug and Review are proposed task envelopes with visible instructions, models, tool limits and delegation policy.",
    "action": "Enter selects",
    "primaryAction": "Enter selects",
    "controls": [
      "A-01",
      "A-02",
      "A-03",
      "A-04",
      "A-05",
      "A-06",
      "A-07",
      "A-08",
      "A-09",
      "A-10",
      "A-11",
      "A-12"
    ],
    "surface": "quickpick",
    "designPage": 118,
    "contractPage": 119,
    "layoutRegions": "Agent search · Code / Ask / Plan / Debug / Review rows · Selected agent description, model, permissions · New Custom Agent…",
    "vscodeVariant": "Dark Modern Quick Pick · native.",
    "jetbrainsVariant": "Light popup chooser · native.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser; 32 px rows; selected detail pane.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from the Work agent control; Enter selects; New Custom Agent… opens S07; Escape restores the previous preset.",
    "traceability": "Design p.118; contract p.119; controls `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`."
  },
  {
    "id": "S07",
    "title": "Custom agent",
    "group": "Models and providers",
    "job": "Models and providers. Build a role from bounded instructions, model settings, tools and parent-owned budget. Preview the resulting envelope before",
    "action": "Save Agent · JetBrains: OK / Apply",
    "primaryAction": "Save Agent · JetBrains: OK / Apply",
    "controls": [
      "M-05",
      "A-04",
      "A-05",
      "A-06",
      "A-07",
      "A-08",
      "A-09",
      "A-11",
      "A-12"
    ],
    "surface": "settings",
    "designPage": 120,
    "contractPage": 121,
    "layoutRegions": "Agent name and instructions · Model and tools · Concurrency and per-agent budget · Delegation policy · Save Agent / Cancel",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; bounded instruction field; envelope preview.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Agents > agent; Save Agent previews the envelope and saves; Cancel keeps the previous preset.",
    "traceability": "Design p.120; contract p.121; controls `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`."
  },
  {
    "id": "S08",
    "title": "Context",
    "group": "Context",
    "job": "Context. Inspect what enters a call, what it costs in estimated or observed tokens, and where it came from. Capacity and input usage have",
    "action": "Open Sources",
    "primaryAction": "Open Sources",
    "controls": [
      "C-01",
      "C-02",
      "C-04",
      "C-05",
      "C-07",
      "C-08",
      "C-09",
      "C-10",
      "C-11",
      "C-12",
      "C-13",
      "C-14"
    ],
    "surface": "sidebar",
    "designPage": 123,
    "contractPage": 124,
    "layoutRegions": "Sent / reserved / capacity header · Category breakdown: system, rules, tools, conversation, files, retrieval · Measurement basis and safety reserve · Open Sources / Export…",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Overview > Context or composer + Context; Open Sources opens S09; Export… exports the bounded context view; Back to Work returns.",
    "traceability": "Design p.123; contract p.124; controls `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`."
  },
  {
    "id": "S62",
    "title": "Unavailable context usage",
    "group": "Context",
    "job": "Context. Render null authoritative context usage as unavailable and keep any draft estimate on a separate basis.",
    "action": "Open Sources",
    "primaryAction": "Open Sources",
    "controls": [
      "C-01",
      "C-02"
    ],
    "surface": "sidebar",
    "designPage": 125,
    "contractPage": 126,
    "layoutRegions": "Last-call header · Sent: unavailable · Model capacity · Draft estimate on a separate basis · Open Sources / Back to Work",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Last-call usage unavailable; no percentage guess; draft estimate is labelled separately.",
    "navigation": "Overview > Context when usage is null; Open Sources opens S09; Back to Work returns without inventing a percentage.",
    "traceability": "Design p.125; contract p.126; controls `C-01 C-02`."
  },
  {
    "id": "S09",
    "title": "Add context",
    "group": "Context",
    "job": "Context. A single discoverable picker adds exact files, ranges, diagnostics, terminal excerpts, diffs, URLs and prior-chat references to a",
    "action": "Enter selects · Add 3 Items",
    "primaryAction": "Enter selects · Add 3 Items",
    "controls": [
      "C-03",
      "C-04",
      "C-05",
      "C-06",
      "C-07",
      "C-08",
      "C-13",
      "X-10",
      "X-11"
    ],
    "surface": "quickpick",
    "designPage": 127,
    "contractPage": 128,
    "layoutRegions": "Search field · Source rows with type/range/status · Selection count · Unsupported-image explanation · Add 3 Items",
    "vscodeVariant": "Dark Modern Quick Pick · native.",
    "jetbrainsVariant": "Light popup chooser · native.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. VS Code Quick Pick / JetBrains popup chooser with multi-select; 32 px rows.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from composer + Context or S08 Open Sources; Enter selects, Add 3 Items adds; Escape cancels and preserves the draft.",
    "traceability": "Design p.127; contract p.128; controls `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`."
  },
  {
    "id": "S10",
    "title": "Compact context",
    "group": "Context",
    "job": "Context. Compaction preserves task obligations and operation identity while reducing eligible history. Show a plan before dispatch and a",
    "action": "Compact",
    "primaryAction": "Compact",
    "controls": [
      "C-12",
      "K-01",
      "K-02",
      "K-03",
      "K-04",
      "I-11",
      "V-07"
    ],
    "surface": "modal",
    "designPage": 129,
    "contractPage": 130,
    "layoutRegions": "Now vs after estimate · Always-kept list: task, criteria, constraints, candidate, evidence, permissions, approvals · Summary-model route and pricing/data boundary · Compact / Show Details / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from Context/compaction threshold Review; Compact starts a durable operation; Show Details expands always-kept facts; Cancel keeps the prior basis.",
    "traceability": "Design p.129; contract p.130; controls `K-01 K-02 K-03 K-04 C-12 I-11 V-07`."
  },
  {
    "id": "S11",
    "title": "Instructions & rules",
    "group": "Instructions and memory",
    "job": "Instructions and memory. Inspect instructions by origin and activation scope, edit allowed sources and preview the merged bundle that the next task will",
    "action": "Add Rule… · JetBrains: OK / Apply",
    "primaryAction": "Add Rule… · JetBrains: OK / Apply",
    "controls": [
      "I-01",
      "I-02",
      "I-03",
      "I-04",
      "I-05",
      "I-06",
      "I-07",
      "I-08",
      "I-09",
      "I-10",
      "I-11"
    ],
    "surface": "settings",
    "designPage": 133,
    "contractPage": 134,
    "layoutRegions": "Active instructions grouped by origin/scope · Output style · Preview Combined Instructions · Add Rule… / Import Rules…",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; origin/scope rows; merged preview disclosure.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Instructions; Add Rule/Import open bounded editors; Preview Combined Instructions opens the merged bundle; OK/Apply saves.",
    "traceability": "Design p.133; contract p.134; controls `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`."
  },
  {
    "id": "S69",
    "title": "Instruction precedence",
    "group": "Instructions and memory",
    "job": "Instructions and memory. Explain instruction precedence and managed policy conflicts without treating project text as executable authority.",
    "action": "Open Project Instructions · JetBrains: OK / Apply",
    "primaryAction": "Open Project Instructions · JetBrains: OK / Apply",
    "controls": [
      "I-04",
      "I-08",
      "Q-12"
    ],
    "surface": "settings",
    "designPage": 135,
    "contractPage": 136,
    "layoutRegions": "Precedence ladder (highest first) · Locked/conflict markers · Policy explanation and resolution · Open Project Instructions / Back",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; precedence ladder; locked/conflict rows.",
    "states": "Organization policy locked; project instruction conflicts; blocked upload does not run.",
    "navigation": "Settings > Faktor > Instructions > Precedence; Open Project Instructions opens the source; Back returns to the precedence ladder.",
    "traceability": "Design p.135; contract p.136; controls `I-04 I-08 Q-12`."
  },
  {
    "id": "S12",
    "title": "Memory",
    "group": "Instructions and memory",
    "job": "Instructions and memory. Separate learned failure priors, user instructions and proposed personal/project memory. Each record has scope, source, date",
    "action": "Save Memory · JetBrains: OK / Apply",
    "primaryAction": "Save Memory · JetBrains: OK / Apply",
    "controls": [
      "K-05",
      "K-10",
      "K-11",
      "I-11",
      "O-09"
    ],
    "surface": "settings",
    "designPage": 137,
    "contractPage": 138,
    "layoutRegions": "New-memory policy · Saved memory rows with source/date · Deletion semantics · Save Memory / Delete…",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; versioned memory rows; delete disclosure.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Memory; Save Memory persists; Delete… opens S66; OK/Apply returns to Settings.",
    "traceability": "Design p.137; contract p.138; controls `K-05 K-10 K-11 O-09 I-11`."
  },
  {
    "id": "S66",
    "title": "Memory deletion",
    "group": "Instructions and memory",
    "job": "Instructions and memory. Delete one versioned memory from future retrieval while disclosing earlier prompts, artifacts and log copies.",
    "action": "Delete",
    "primaryAction": "Delete",
    "controls": [
      "K-10",
      "K-11"
    ],
    "surface": "modal",
    "designPage": 139,
    "contractPage": 140,
    "layoutRegions": "Delete-memory confirmation · Scope and future-retrieval effect · Earlier prompt/artifact/log copies warning · Delete / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; delete confirmation names the object.",
    "states": "Delete stops future use; earlier prompts/artifacts/logs are not removed; user memory/failure learning unaffected.",
    "navigation": "Opened from S12 Delete…; Delete removes future retrieval only; Cancel returns to Memory.",
    "traceability": "Design p.139; contract p.140; controls `K-10 K-11`."
  },
  {
    "id": "S13",
    "title": "Permissions",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Explain effective permissions by tool, action, target and scope. Show persistent rules separately from current pending approvals",
    "action": "Apply Policy · JetBrains: OK / Apply",
    "primaryAction": "Apply Policy · JetBrains: OK / Apply",
    "controls": [
      "Q-01",
      "Q-02",
      "Q-03",
      "Q-04",
      "Q-05",
      "Q-07",
      "Q-08",
      "Q-09",
      "Q-10",
      "Q-11",
      "Q-12",
      "Q-13",
      "Q-14"
    ],
    "surface": "settings",
    "designPage": 142,
    "contractPage": 143,
    "layoutRegions": "Permission preset · Per-action rows: read, edit candidate, run shell, write outside workspace · Files/network containment · Organization lock explanation · Test a Rule… / Apply Policy",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; action/target/scope matrix; locked rows.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Permissions; Apply Policy saves a validated policy; Test a Rule opens the simulator; organization locks remain visible.",
    "traceability": "Design p.142; contract p.143; controls `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`."
  },
  {
    "id": "S15",
    "title": "Tools & MCP",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Group built-in tools and MCP servers by origin, availability and active permissions. Make discovered, enabled, healthy and",
    "action": "Add Server… · JetBrains: OK / Apply",
    "primaryAction": "Add Server… · JetBrains: OK / Apply",
    "controls": [
      "T-01",
      "T-02",
      "T-03",
      "T-04",
      "T-06",
      "T-07",
      "T-08",
      "T-09",
      "T-10",
      "T-11",
      "T-12",
      "T-13",
      "T-15"
    ],
    "surface": "settings",
    "designPage": 144,
    "contractPage": 145,
    "layoutRegions": "Server list with health/auth/tool count · Selected server tools with allow/deny · Resources and prompts · Token cost of definitions · Add Server… / Open Logs",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; server health rows; tool policy rows.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Tools & MCP; Add Server opens S16; server rows expose health/auth/tools; Open Logs opens the bounded log view.",
    "traceability": "Design p.144; contract p.145; controls `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`."
  },
  {
    "id": "S16",
    "title": "Add MCP server",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Configure a supervised stdio server with exact command/args and explicit trust. Remote transport is offered only after capability",
    "action": "Save and Start",
    "primaryAction": "Save and Start",
    "controls": [
      "T-03",
      "T-04",
      "T-05",
      "T-06",
      "T-10",
      "T-13",
      "T-14",
      "T-15"
    ],
    "surface": "settings",
    "designPage": 146,
    "contractPage": 147,
    "layoutRegions": "Transport stdio/HTTP · Name, command, arguments, environment · Trust statement · Save and Start / Cancel",
    "vscodeVariant": "Dark Modern editor tab · webview.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor/dialog; stdio/HTTP segmented control; command/args fields.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Opened from S15 Add Server…; Save and Start saves the spec then explicitly starts; Cancel keeps the previous server config.",
    "traceability": "Design p.146; contract p.147; controls `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`."
  },
  {
    "id": "S68",
    "title": "Future MCP authentication",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Specify future remote MCP authentication with exact origin, scopes, token storage and cancellation behavior.",
    "action": "Open Sign-In Page",
    "primaryAction": "Open Sign-In Page",
    "controls": [
      "T-04",
      "T-06"
    ],
    "surface": "modal",
    "designPage": 148,
    "contractPage": 149,
    "layoutRegions": "Server origin and scopes · Read/write access · Token storage (machine vault) · Browser sign-in explanation · Open Sign-In Page / Cancel",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Modal message / DialogWrapper; origin/scopes/token storage.",
    "states": "Sign-in pending/cancelled; token stored in machine vault; cancellation keeps server settings.",
    "navigation": "Opened from S15 server Sign In…; Open Sign-In Page starts browser auth; Cancel keeps server settings and does not connect.",
    "traceability": "Design p.148; contract p.149; controls `T-04 T-06`."
  },
  {
    "id": "S17",
    "title": "Skills & commands",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Discover reusable workflows without confusing installation with permission grants. Commands say whether they insert text, start",
    "action": "Install Skill… · JetBrains: OK / Apply",
    "primaryAction": "Install Skill… · JetBrains: OK / Apply",
    "controls": [
      "I-06",
      "S-01",
      "S-02",
      "S-03",
      "S-04",
      "S-05",
      "S-06",
      "S-14"
    ],
    "surface": "settings",
    "designPage": 150,
    "contractPage": 151,
    "layoutRegions": "Skills list with scope/invocation · Commands list with action type · New Command… · Install Skill…",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; skill and command lists.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Skills & Hooks; Install Skill… and New Command… open bounded editors; OK/Apply saves.",
    "traceability": "Design p.150; contract p.151; controls `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`."
  },
  {
    "id": "S18",
    "title": "Lifecycle hook",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Edit exact native lifecycle events, executable arguments, environment, bounds and failure policy. Test under the same capability",
    "action": "Save Hook · JetBrains: OK / Apply",
    "primaryAction": "Save Hook · JetBrains: OK / Apply",
    "controls": [
      "S-07",
      "S-08",
      "S-09",
      "S-10",
      "S-11",
      "S-12",
      "S-13",
      "S-14"
    ],
    "surface": "settings",
    "designPage": 152,
    "contractPage": 153,
    "layoutRegions": "Hook event · Command · Timeout and output limit · Failure policy · Allowed environment variables · Last test result · Save Hook / Test Hook",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; event, command, bounds and failure policy.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Skills & Hooks > hook; Test Hook opens S67; Save Hook saves a validated version; Cancel keeps the previous hook.",
    "traceability": "Design p.152; contract p.153; controls `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`."
  },
  {
    "id": "S67",
    "title": "Hook test result",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Inspect a real bounded hook test, deadline, output and verdict without converting successful output into authority.",
    "action": "Back to Hook",
    "primaryAction": "Back to Hook",
    "controls": [
      "S-12",
      "S-13"
    ],
    "surface": "modal",
    "designPage": 154,
    "contractPage": 155,
    "layoutRegions": "Result and exit code · Time and policy verdict · Bounded output · Back to Hook / Show Output / Run Again",
    "vscodeVariant": "Dark Modern modal message · native.",
    "jetbrainsVariant": "Light DIALOGWRAPPER.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Passed exit 0 in 128 ms; policy unchanged; passing hook cannot grant authority.",
    "navigation": "Opened from S18 Test Hook; Back to Hook returns to the editor; Run Again repeats the bounded test.",
    "traceability": "Design p.154; contract p.155; controls `S-12 S-13`."
  },
  {
    "id": "S19",
    "title": "Terminal",
    "group": "Permissions and tools",
    "job": "Permissions and tools. An owned process table and bounded output coexist with the task transcript. Terminal kill, run stop and session abort are separate",
    "action": "Open Full Output",
    "primaryAction": "Open Full Output",
    "controls": [
      "X-01",
      "X-02",
      "X-03",
      "X-04",
      "X-05",
      "X-12",
      "B-10",
      "D-07"
    ],
    "surface": "terminal",
    "designPage": 156,
    "contractPage": 157,
    "layoutRegions": "Terminal tabs and owned process list · Bounded output · Full-output link · Stop Process · Filesystem/network scope",
    "vscodeVariant": "Dark Modern panel terminal · native.",
    "jetbrainsVariant": "Light tool window · console.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native terminal/console panel; bounded output; process tabs; 64 KiB visible tail.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Host terminal/console surface owned by the task; Open Full Output opens the bounded artifact; Stop Process acts only on the owned process.",
    "traceability": "Design p.156; contract p.157; controls `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`."
  },
  {
    "id": "S20",
    "title": "Browser",
    "group": "Permissions and tools",
    "job": "Permissions and tools. Integrate host diagnostics and explicit code actions. Browser controls require an actual advertised adapter with account, machine",
    "action": "Capture Screenshot",
    "primaryAction": "Capture Screenshot",
    "controls": [
      "C-03",
      "X-06",
      "X-07",
      "X-08",
      "X-09",
      "X-10",
      "X-11",
      "X-13"
    ],
    "surface": "browser",
    "designPage": 158,
    "contractPage": 159,
    "layoutRegions": "Preview URL · Rendered page · Console/network counts · Capture Screenshot / Add Diagnostics",
    "vscodeVariant": "Dark Modern simple browser · native.",
    "jetbrainsVariant": "Light JCEF preview window.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar/editor surface follows the host chrome; controls use host-native patterns.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Host browser/preview surface opened by an explicit adapter action; Capture Screenshot produces task evidence; Add Diagnostics opens host diagnostics.",
    "traceability": "Design p.158; contract p.159; controls `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`."
  },
  {
    "id": "S21",
    "title": "Budgets & retries",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Show exact run limits and current accounting beside reliability controls. A soft warning is different from enforced admission",
    "action": "Apply Defaults · JetBrains: OK / Apply",
    "primaryAction": "Apply Defaults · JetBrains: OK / Apply",
    "controls": [
      "B-01",
      "B-02",
      "B-03",
      "B-04",
      "B-05",
      "B-06",
      "B-07",
      "B-08",
      "B-09",
      "B-10",
      "B-11",
      "B-12"
    ],
    "surface": "settings",
    "designPage": 161,
    "contractPage": 162,
    "layoutRegions": "Defaults vs running-task limits · Token limit and spending limit · Spent vs held · Retry attempts and backoff · Advanced reliability disclosure · Apply Defaults / Reset",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; caps and retry fields; held/spent row.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Budgets; Apply Defaults applies to new tasks; Reset restores defaults; running tasks keep captured limits.",
    "traceability": "Design p.161; contract p.162; controls `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`."
  },
  {
    "id": "S28",
    "title": "Efficiency",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Expose current production efficiency flags and their off-switches with honest measurement, fallback and apply timing.",
    "action": "Apply · JetBrains: OK / Apply",
    "primaryAction": "Apply · JetBrains: OK / Apply",
    "controls": [
      "M-04",
      "M-06",
      "M-10",
      "C-09",
      "C-10",
      "C-11",
      "K-05",
      "K-06",
      "K-07",
      "K-08",
      "K-09",
      "K-12"
    ],
    "surface": "settings",
    "designPage": 163,
    "contractPage": 164,
    "layoutRegions": "Efficiency flags with explanations · Routing policy · Search without embeddings · Apply / Reset to Defaults",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; flags with explanations.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Efficiency; Apply changes routing flags; Reset to Defaults restores production defaults; no fake measured effect.",
    "traceability": "Design p.163; contract p.164; controls `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`."
  },
  {
    "id": "S29",
    "title": "General settings",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Search settings across allowed scopes and inspect each effective value, source, lock and timing. Distinguish staged, saved and",
    "action": "VS Code: Settings saves valid values · JetBrains: OK / Apply",
    "primaryAction": "VS Code: Settings saves valid values · JetBrains: OK / Apply",
    "controls": [
      "D-09",
      "D-10",
      "D-11",
      "D-14",
      "O-01",
      "O-04",
      "O-07",
      "O-08",
      "O-10",
      "O-12",
      "O-13"
    ],
    "surface": "native-settings",
    "designPage": 165,
    "contractPage": 166,
    "layoutRegions": "Native settings search · Settings list with effective values · Scope/lock/timing · OK / Cancel / Apply",
    "vscodeVariant": "Dark Modern settings editor · native.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native Settings editor; search field; scope/provenance list.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Native Settings > Faktor or Settings > Faktor; OK/Apply commits valid values; staged/saved/runtime-applied states remain distinct.",
    "traceability": "Design p.165; contract p.166; controls `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`."
  },
  {
    "id": "S64",
    "title": "Settings search and provenance",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Find persistent settings in the native host, inspect scope/provenance and remove an override deliberately.",
    "action": "VS Code: Settings saves valid values · JetBrains: OK / Apply",
    "primaryAction": "VS Code: Settings saves valid values · JetBrains: OK / Apply",
    "controls": [
      "D-11",
      "O-01"
    ],
    "surface": "native-settings",
    "designPage": 167,
    "contractPage": 168,
    "layoutRegions": "Settings search results · Provenance: user/workspace/organization · Override/reset action · OK / Apply",
    "vscodeVariant": "Dark Modern settings editor · native.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Native Settings editor; search results; provenance and reset.",
    "states": "3 settings found; workspace override vs user default; organization lock; reset to inherited.",
    "navigation": "Native Settings search; selecting a result opens its provenance; Reset to Inherited Value removes the override.",
    "traceability": "Design p.167; contract p.168; controls `O-01 D-11`."
  },
  {
    "id": "S60",
    "title": "Settings revision conflict",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Resolve a rejected configuration patch against the new effective revision without losing staged edits.",
    "action": "Save Resolved Settings · JetBrains: OK / Apply",
    "primaryAction": "Save Resolved Settings · JetBrains: OK / Apply",
    "controls": [
      "M-12",
      "O-01"
    ],
    "surface": "settings",
    "designPage": 169,
    "contractPage": 170,
    "layoutRegions": "Revision-conflict banner · Your value vs now-saved value · Keep my value / Use saved value · Save Resolved Settings / Cancel",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; conflict banner; per-field choice.",
    "states": "Settings changed while editing; staged values kept; conflicting field must be resolved.",
    "navigation": "Opened when a settings patch conflicts; Save Resolved Settings applies the chosen values; Cancel keeps staged edits.",
    "traceability": "Design p.169; contract p.170; controls `O-01 M-12`."
  },
  {
    "id": "S55",
    "title": "Optional control-plane account",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Connect a machine-scoped optional account with write-only secrets while local coding remains independent.",
    "action": "Connect Account · JetBrains: OK / Apply",
    "primaryAction": "Connect Account · JetBrains: OK / Apply",
    "controls": [
      "P-04",
      "O-07"
    ],
    "surface": "settings",
    "designPage": 171,
    "contractPage": 172,
    "layoutRegions": "Optional-account explanation · Server and organization · Write-only token reference · Connection status · Connect Account / Cancel",
    "vscodeVariant": "Dark Modern editor tab · webview SETTINGS.",
    "jetbrainsVariant": "Light settings dialog · Configurable.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Settings editor / Configurable; optional account fields; write-only token.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Settings > Faktor > Account; Connect Account starts the optional control-plane flow; Cancel keeps local-only operation.",
    "traceability": "Design p.171; contract p.172; controls `O-07 P-04`."
  },
  {
    "id": "S30",
    "title": "Diagnostics",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Show readiness, version compatibility, runtime identity and bounded diagnostics with an actionable typed cause.",
    "action": "Preview Export…",
    "primaryAction": "Preview Export…",
    "controls": [
      "B-11",
      "B-12",
      "D-13",
      "O-03",
      "O-04",
      "O-05",
      "O-07",
      "O-09",
      "O-13"
    ],
    "surface": "sidebar",
    "designPage": 173,
    "contractPage": 174,
    "layoutRegions": "Service/version status · Executable/checksum/signature · Model provider state · Code index state · Export options: versions, capabilities, error excerpts, source content · Preview / Export… / Open Logs",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar diagnostics; export options.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Diagnostics from the Work status/More menu or Settings > Faktor > Diagnostics; Preview/Export… open bounded artifacts; Open Logs opens host logs.",
    "traceability": "Design p.173; contract p.174; controls `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`."
  },
  {
    "id": "S56",
    "title": "Protocol-blocked recovery",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Preserve the last coherent snapshot and cursor when a typed stream refusal pauses automatic recovery.",
    "action": "Restart Service…",
    "primaryAction": "Restart Service…",
    "controls": [
      "D-13",
      "O-04"
    ],
    "surface": "sidebar",
    "designPage": 175,
    "contractPage": 176,
    "layoutRegions": "Out-of-sync banner · Last update and draft retention · Run view age · Expected/received protocol versions · Restart Service… / Copy Diagnostics",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar protocol recovery; expected/received versions.",
    "states": "Live updates paused; last coherent snapshot/cursor retained; run view out of date; restart is explicit.",
    "navigation": "Opened from the stale/live-updates-paused banner; Restart Service… restarts explicitly; Copy Diagnostics copies the typed mismatch.",
    "traceability": "Design p.175; contract p.176; controls `O-04 D-13`."
  },
  {
    "id": "S31",
    "title": "Workspace binding",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Display editor, execution, model, vault and candidate locations together. Remote workspaces must not blur local/hosted",
    "action": "Review New Setup",
    "primaryAction": "Review New Setup",
    "controls": [
      "P-13",
      "A-11",
      "X-13",
      "D-01",
      "D-15",
      "O-02",
      "O-07",
      "O-11"
    ],
    "surface": "sidebar",
    "designPage": 177,
    "contractPage": 178,
    "layoutRegions": "Workspace-moved banner · Where things run: editor, workspace, model endpoint, credentials · Review New Setup / Cancel",
    "vscodeVariant": "Dark Modern sidebar view · webview.",
    "jetbrainsVariant": "Light tool window · Swing/Kotlin UI DSL.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. Sidebar workspace binding; where things run.",
    "states": "Empty: name the add/discover/refresh action; loading: bounded/cancellable; stale: last coherent snapshot with age; error: typed cause/scope/recovery; disabled: missing prerequisite named (pp.13, 222).",
    "navigation": "Diagnostics > Where things run when the workspace binding changes; Review New Setup opens S34; Cancel keeps the old binding marked stale.",
    "traceability": "Design p.177; contract p.178; controls `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`."
  },
  {
    "id": "S58",
    "title": "Narrow Work panel",
    "group": "Settings and diagnostics",
    "job": "Settings and diagnostics. Keep a 320 px Work panel readable with stacked controls, one selected detail and an independently reachable composer.",
    "action": "Queue Message (composer)",
    "primaryAction": "Queue Message (composer)",
    "controls": [
      "D-08",
      "D-09",
      "D-14"
    ],
    "surface": "narrow",
    "designPage": 179,
    "contractPage": 180,
    "layoutRegions": "320 px task header/status · One selected detail · Stacked configuration row · Composer remains reachable",
    "vscodeVariant": "Dark Modern sidebar view at 320 px.",
    "jetbrainsVariant": "Light tool window at 320 px.",
    "visual": "4-step spacing (4/8/16/24); 24 px inset, 156 px label column, 20 px column gap, 36 px control, 8 px helper gap, 16 px row gap; host fonts/theme/focus; 24 px minimum target (buttons 26/28 px); no brand font in reading surfaces; contrast AA and color+text status. 320 px Work panel; stacked controls; primary/attention/composer remain visible; 200% text target.",
    "states": "Narrow 320 px: one selected detail, stacked configuration, composer independently reachable.",
    "navigation": "Work at 320 px; the same navigation as S01, with one selected detail and the composer reachable.",
    "traceability": "Design p.179; contract p.180; controls `D-08 D-09 D-14`."
  }
];

export const E8_CONTROLS: readonly Edition8Control[] = [
  {
    "id": "M-01",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Default provider",
    "control": "Served-instance picker",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S01",
      "S02"
    ],
    "primaryScreen": "S01",
    "defaultState": "Daemon catalog choice",
    "defaultValue": "Daemon catalog choice",
    "scope": "User/application; draft override",
    "applyTiming": "New conversation",
    "behavior": "Validate provider plus model together. If unavailable, keep the previous selection and offer Refresh; never silently use another endpoint.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.defaultProvider",
      "faktor.run.defaultProvider"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-02",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Default model",
    "control": "Searchable model picker",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S01",
      "S02"
    ],
    "primaryScreen": "S36",
    "defaultState": "Served catalog default",
    "defaultValue": "Served catalog default",
    "scope": "User/application; conversation",
    "applyTiming": "New conversation or supported next task",
    "behavior": "Show full model ID, provider and current availability. A change requiring a new session offers Create new conversation with the draft preserved.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.defaultModel",
      "faktor.run.defaultModel"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-03",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Favorites and recent models",
    "control": "Star and recent list",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02"
    ],
    "primaryScreen": "S02",
    "defaultState": "None; local MRU",
    "defaultValue": "None; local MRU",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Favorites refer to provider plus exact model ID. Removed models remain labelled unavailable until deliberately removed.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-04",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Routing policy",
    "control": "Pinned / Economy picker",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S01",
      "S02",
      "S28"
    ],
    "primaryScreen": "S01",
    "defaultState": "Show effective daemon policy",
    "defaultValue": "Show effective daemon policy",
    "scope": "Daemon; captured task",
    "applyTiming": "Next admitted task",
    "behavior": "Pinned must never fall back silently; Economy explains selected and substituted model. Expose the actual route separately from the requested preference.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "routing_mode"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-05",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Per-role model",
    "control": "Planner / coder / verifier / summarizer map",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02",
      "S07"
    ],
    "primaryScreen": "S02",
    "defaultState": "Inherit selected model",
    "defaultValue": "Inherit selected model",
    "scope": "Agent preset; task",
    "applyTiming": "Next child or model call after ack",
    "behavior": "Resolve exact model capabilities and shared budget for each role. A role model is not permission to create a child agent.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-06",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Fallback chain",
    "control": "Ordered allowed candidates",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02",
      "S28"
    ],
    "primaryScreen": "S02",
    "defaultState": "No added fallback",
    "defaultValue": "No added fallback",
    "scope": "Task/daemon policy",
    "applyTiming": "Next routing decision",
    "behavior": "Explain trigger, price/data-boundary change and quality floor. Do not route local-only work to a hosted endpoint. Exhaustion is a visible failure.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 257
  },
  {
    "id": "M-07",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Model capability sheet",
    "control": "Read-only badges and details",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02"
    ],
    "primaryScreen": "S02",
    "defaultState": "Served capability authority",
    "defaultValue": "Served capability authority",
    "scope": "Provider/model",
    "applyTiming": "On catalog refresh",
    "behavior": "Show context, output, tools, parallel tools, reasoning, thinking, vision, documents, streaming and structured output. Unknown, generic declaration and exact-model authority remain distinct.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-08",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Catalog refresh and probe",
    "control": "Refresh / probe exact model",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02"
    ],
    "primaryScreen": "S02",
    "defaultState": "No automatic destructive probe",
    "defaultValue": "No automatic destructive probe",
    "scope": "Daemon instance",
    "applyTiming": "On explicit action",
    "behavior": "Show last observation time and probe failure. A context allocation probe does not certify tool-call or coding reliability.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-09",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Context-window override",
    "control": "Positive integer, capability-bounded",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02",
      "S04"
    ],
    "primaryScreen": "S02",
    "defaultState": "Served effective limit",
    "defaultValue": "Served effective limit",
    "scope": "Model declaration; operator",
    "applyTiming": "After config validation and ack",
    "behavior": "Lower limits may constrain a model; larger declarations need supported authority. Show configured versus runtime-observed limits and reject impossible reservations.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "model.contextLimit",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-10",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Quality floors",
    "control": "Advanced declared thresholds",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02",
      "S28"
    ],
    "primaryScreen": "S02",
    "defaultState": "Effective daemon floors",
    "defaultValue": "Effective daemon floors",
    "scope": "Operator/task policy",
    "applyTiming": "Next route",
    "behavior": "Show user-declared versus observed quality and sample counts. Never relabel a manually entered score as a measured benchmark.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "provider.quality",
      "(per-instance)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-11",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Actual model per call",
    "control": "Read-only call identity",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S23",
      "S02"
    ],
    "primaryScreen": "S22",
    "defaultState": "No call yet",
    "defaultValue": "No call yet",
    "scope": "Turn/call",
    "applyTiming": "On durable observation",
    "behavior": "Display provider, exact model, route reason and role on expanded calls. A composer preference must not masquerade as the model that executed.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-12",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Switch timing",
    "control": "Next task / supported next turn",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S02",
      "S60"
    ],
    "primaryScreen": "S02",
    "defaultState": "Current call unchanged",
    "defaultValue": "Current call unchanged",
    "scope": "Draft/session",
    "applyTiming": "After explicit acknowledgement",
    "behavior": "Changing a picker never rewrites an in-flight request. Unsupported mid-session changes offer a new session; show when the new value actually applies.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-13",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Cost and limit details",
    "control": "Read-only price/limit sheet",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S53",
      "S27",
      "S02"
    ],
    "primaryScreen": "S53",
    "defaultState": "Unavailable if not served",
    "defaultValue": "Unavailable if not served",
    "scope": "Model/provider",
    "applyTiming": "At quote or observation",
    "behavior": "Separate list/declared/observed price and quota. Cache prices may be absent; missing rates do not become zero. No hard-coded model price recommendation.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 258
  },
  {
    "id": "M-14",
    "category": "Models and routing",
    "categoryCode": "M",
    "name": "Model comparison",
    "control": "Bounded side-by-side candidate run",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S51",
      "S02",
      "S63"
    ],
    "primaryScreen": "S51",
    "defaultState": "Off",
    "defaultValue": "Off",
    "scope": "Task",
    "applyTiming": "New comparison admission",
    "behavior": "Name all candidate models, shared cap and evidence basis before running. Compare verified outcomes and attempted spend; keep candidate identity and user choice explicit.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 259
  },
  {
    "id": "P-01",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Connection label and instance ID",
    "control": "Label + immutable unique ID",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S34",
      "S03"
    ],
    "primaryScreen": "S34",
    "defaultState": "User supplied label",
    "defaultValue": "User supplied label",
    "scope": "Machine/daemon",
    "applyTiming": "After save",
    "behavior": "Show readable label and reveal exact ID. Validate uniqueness before storing. Renaming a label must not move credentials or alias a different endpoint.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "providers[].id"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-02",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Provider/API family",
    "control": "Capability-supported family picker",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "Detected suggestion, user confirmed",
    "defaultValue": "Detected suggestion, user confirmed",
    "scope": "Connection",
    "applyTiming": "After save/restart if required",
    "behavior": "Distinguish OpenAI Chat, Responses and other actually supported adapters. Never infer a remote API family from a marketing model name.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "providers[].kind"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-03",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Base URL",
    "control": "Validated endpoint field",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "Adapter default, shown explicitly",
    "defaultValue": "Adapter default, shown explicitly",
    "scope": "Machine/connection",
    "applyTiming": "After validation and connection ack",
    "behavior": "Show which machine resolves the URL. Reject invalid schemes, userinfo and blocked destinations; keep refusal inline without exposing credential fragments.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "providers[].base_url"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-04",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Credential reference",
    "control": "Secret-store or environment-name selector",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03",
      "S55"
    ],
    "primaryScreen": "S03",
    "defaultState": "No credential value",
    "defaultValue": "No credential value",
    "scope": "Machine secret store",
    "applyTiming": "On next authorized request",
    "behavior": "Current provider config uses environment references. Direct IDE provider-key entry requires a secure daemon handoff contract; never put key values in shared settings or exports.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "providers[].api_key_env"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-05",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Custom headers",
    "control": "Bounded name + secret-reference rows",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "None",
    "defaultValue": "None",
    "scope": "Connection, secret values in vault",
    "applyTiming": "After save",
    "behavior": "Current adapter helper bounds: 32 headers, names 128 bytes, values 8192 bytes. Native editor persistence and vault binding still need implementation. Mask auth-shaped values.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "providers[].headers",
      "(adapter",
      "ExtraHeaders)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-06",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Models on custom endpoint",
    "control": "Fetch models / add exact ID",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "Served or explicitly declared IDs",
    "defaultValue": "Served or explicitly declared IDs",
    "scope": "Connection",
    "applyTiming": "After catalog reload",
    "behavior": "Display live, exact declaration or generic declaration provenance. Failed discovery does not erase explicit IDs or falsely validate tool calling.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "providers[].models"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 260
  },
  {
    "id": "P-07",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Local engine choice",
    "control": "Ollama / compatible endpoint",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04"
    ],
    "primaryScreen": "S04",
    "defaultState": "Existing configured local adapter",
    "defaultValue": "Existing configured local adapter",
    "scope": "Connection",
    "applyTiming": "After readiness",
    "behavior": "Expose the protocol being used, not a promise of support for every inference server. LM Studio/vLLM are endpoint examples only when that compatible path is configured.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "providers[].kind=ollama"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-08",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Loopback permission",
    "control": "Explicit local-endpoint opt-in",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04"
    ],
    "primaryScreen": "S04",
    "defaultState": "Adapter default; show actual value",
    "defaultValue": "Adapter default; show actual value",
    "scope": "Machine/connection",
    "applyTiming": "After validated config reload",
    "behavior": "Ollama defaults to permitted loopback; custom OpenAI uses explicit opt-in. This switch does not authorize private LAN, link-local or metadata destinations.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "providers[].allow_loopback"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-09",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Private LAN inference",
    "control": "Trusted destination policy editor",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04"
    ],
    "primaryScreen": "S04",
    "defaultState": "Not available under current provider rule",
    "defaultValue": "Not available under current provider rule",
    "scope": "Machine/operator policy",
    "applyTiming": "Only after new backend policy exists",
    "behavior": "A factory-LAN address cannot be enabled by allow_loopback. Show the current typed refusal; scoped authenticated LAN policy is new work, not a hidden workaround toggle.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "sandbox.network",
      "(proposed",
      "LAN",
      "policy)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-10",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Connection test",
    "control": "Test auth / list / minimal capability probe",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "No request until selected",
    "defaultValue": "No request until selected",
    "scope": "Connection",
    "applyTiming": "On explicit action",
    "behavior": "Show stages and target; note if the chosen probe incurs provider usage. Auth OK, model listed and model capable are separate results.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-11",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Transport timeout",
    "control": "Positive duration within adapter range",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "Effective adapter policy",
    "defaultValue": "Effective adapter policy",
    "scope": "Connection",
    "applyTiming": "Next request after ack",
    "behavior": "Show connect, first-byte and total deadlines where supported. Do not retry a request whose external effect or streamed admission is unknown.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "provider",
      "transport",
      "timeout",
      "(adapter)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-12",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Proxy, TLS and certificates",
    "control": "Managed options with validation",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "System/daemon policy",
    "defaultValue": "System/daemon policy",
    "scope": "Machine/operator",
    "applyTiming": "After reload",
    "behavior": "Show effective proxy and certificate source without secrets. Do not provide a convenient disable-TLS- verification checkbox as an ordinary setting.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-13",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Remote execution location",
    "control": "Read-only host and locality badges",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04",
      "S31"
    ],
    "primaryScreen": "S04",
    "defaultState": "Current daemon host",
    "defaultValue": "Current daemon host",
    "scope": "Connection/session",
    "applyTiming": "On binding",
    "behavior": "Local means local to the daemon, not necessarily the laptop. SSH/container sessions show endpoint reachability, vault location and file-resolution host.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 261
  },
  {
    "id": "P-14",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Reconnect, rotate and delete",
    "control": "Explicit connection lifecycle actions",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S03"
    ],
    "primaryScreen": "S03",
    "defaultState": "Keep active binding until replaced",
    "defaultValue": "Keep active binding until replaced",
    "scope": "Machine/connection",
    "applyTiming": "After acknowledgement",
    "behavior": "Rotation is write-only. Delete shows affected drafts and sessions; active requests retain their captured credential generation, with clear failure if revoked.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 262
  },
  {
    "id": "P-15",
    "category": "Providers, credentials and local inference",
    "categoryCode": "P",
    "name": "Local allocation and keep-alive",
    "control": "Adapter-offered context allocation / keep-alive / unload",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04"
    ],
    "primaryScreen": "S04",
    "defaultState": "Inherit runtime policy",
    "defaultValue": "Inherit runtime policy",
    "scope": "Local connection/operator",
    "applyTiming": "After explicit runtime acknowledgement",
    "behavior": "Show configured versus observed allocation. Offer only supported engine controls; unloading needs ownership and active-use checks. No generic GPU or thread sliders without a real engine contract.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "ollama.keep_alive"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 262
  },
  {
    "id": "G-01",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Reasoning effort",
    "control": "Served enum; Off/Low/Medium/High when mapped",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S01",
      "S05"
    ],
    "primaryScreen": "S01",
    "defaultState": "Inherit model/daemon",
    "defaultValue": "Inherit model/daemon",
    "scope": "Model default; draft/task override",
    "applyTiming": "Next admitted call",
    "behavior": "Use the adapter-offered values. Current core enum has four levels; do not expose Max or a made-up numeric range for every model.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "model.reasoning",
      "(core",
      "enum)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-02",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Thinking token budget",
    "control": "Integer bounded by model capability",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Provider/model default",
    "defaultValue": "Provider/model default",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Keep thinking budget separate from total output reserve. Hide if the adapter cannot control it; explain any shared reasoning/output accounting.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-03",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Maximum output",
    "control": "Positive token limit",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Served maxOutput",
    "defaultValue": "Served maxOutput",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Validate against context and output capabilities. Increasing output reserve reduces space for input; warn before discarding or compacting selected context.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "task.max_tokens"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-04",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Temperature",
    "control": "Adapter-offered numeric range",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Provider default; unset stays unset",
    "defaultValue": "Provider default; unset stays unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Unsupported models show Not supported rather than a nonfunctional slider. Default reset removes the override; never send a null or arbitrary zero accidentally.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-05",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Top-p",
    "control": "Adapter-supported probability value",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Unset",
    "defaultValue": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Validate the exact adapter range. Warn on mutually restrictive sampling choices; preserve provider defaults unless the user sets a value.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-06",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Top-k",
    "control": "Adapter-supported integer",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Unset",
    "defaultValue": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Only show when the selected adapter accepts it. A local runtime option is not automatically a hosted-model capability.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 263
  },
  {
    "id": "G-07",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Frequency/presence penalties",
    "control": "Separate supported values",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Unset",
    "defaultValue": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Render independent labels, ranges and reset actions. Unsupported request fields must be rejected before submission, not silently ignored.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-08",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Seed and determinism",
    "control": "Optional integer + caveat",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Unset",
    "defaultValue": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "A seed is best-effort provider behavior, not reproducibility proof. Show whether the provider acknowledged it and retain it in the captured request summary.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-09",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Stop sequences",
    "control": "Bounded list of strings",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "None",
    "defaultValue": "None",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Warn when stops can truncate structured tool output. Do not allow an advanced text setting to corrupt native tool-call framing.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-10",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Parallel model tool calls",
    "control": "Capability-gated switch and cap",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Effective supported policy",
    "defaultValue": "Effective supported policy",
    "scope": "Agent/task",
    "applyTiming": "Next model call",
    "behavior": "Needs both model support and runtime-safe scheduling. Concurrency must not bypass shared budget, capability grants, cancellation or edit ownership.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-11",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Output format/schema",
    "control": "Text / supported structured schema",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Normal agent protocol",
    "defaultValue": "Normal agent protocol",
    "scope": "Task",
    "applyTiming": "Next supported operation",
    "behavior": "Validate schema size and compatibility. Do not replace Faktor internal tool envelopes with user-supplied formatting or claim JSON merely because prose looks structured.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-12",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Service tier and cache policy",
    "control": "Adapter-supported options",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Provider default",
    "defaultValue": "Provider default",
    "scope": "Connection/task policy",
    "applyTiming": "Next call",
    "behavior": "Display actual tier and cache-write/read basis. A cache toggle must not claim that a prefix hit occurred; observe that separately.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-13",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Stream responses",
    "control": "Supported on/off preference",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S05"
    ],
    "primaryScreen": "S05",
    "defaultState": "Stream when supported",
    "defaultValue": "Stream when supported",
    "scope": "User/model",
    "applyTiming": "Next call",
    "behavior": "Preserve stop, stale-state and transcript semantics when disabled. A quiet model call is not an idle task; show request phase without fake progress.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "streaming",
      "(adapter)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 264
  },
  {
    "id": "G-14",
    "category": "Reasoning and generation",
    "categoryCode": "G",
    "name": "Effective request preview",
    "control": "Read-only normalized summary",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S05"
    ],
    "primaryScreen": "S22",
    "defaultState": "No request captured yet",
    "defaultValue": "No request captured yet",
    "scope": "Draft and immutable call receipt",
    "applyTiming": "Before admission / after ack",
    "behavior": "List requested, effective and unsupported fields with source and timing. Do not dump hidden system secrets or claim to expose private model reasoning.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 265
  },
  {
    "id": "A-01",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Agent preset picker",
    "control": "Code / Ask / Plan / Debug / Review / Custom",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S01",
      "S06"
    ],
    "primaryScreen": "S36",
    "defaultState": "Code as proposed initial preset",
    "defaultValue": "Code as proposed initial preset",
    "scope": "User default; draft",
    "applyTiming": "Before admission",
    "behavior": "Describe scope and tool envelope in the picker. Existing Task/Ask controls are not equivalent to enforced read-only Ask; the stricter preset needs backend enforcement.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-02",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Ask policy",
    "control": "Read-only enforced envelope",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S06"
    ],
    "primaryScreen": "S06",
    "defaultState": "No writes or effectful tools",
    "defaultValue": "No writes or effectful tools",
    "scope": "Preset/task",
    "applyTiming": "At task admission",
    "behavior": "Allow exploration and answers; filter tools and deny writes at dispatch. Current plain prompt/Ask route alone does not prove read-only operation.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-03",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Plan policy",
    "control": "Exploration + named plan artifact writes",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S06"
    ],
    "primaryScreen": "S06",
    "defaultState": "No code edits",
    "defaultValue": "No code edits",
    "scope": "Preset/task",
    "applyTiming": "At admission",
    "behavior": "Show plan file and review comments. User approval admits a new Code task with a frozen plan version; it does not mutate the planning run in place.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-04",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Custom role instructions",
    "control": "Editor with preview and source",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S06",
      "S07"
    ],
    "primaryScreen": "S06",
    "defaultState": "Inherit baseline instructions",
    "defaultValue": "Inherit baseline instructions",
    "scope": "User/project preset",
    "applyTiming": "Next task",
    "behavior": "Preview the merged instruction bundle and token estimate. Prompt text cannot widen a capability grant or override managed policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-05",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Tool allowlist per preset",
    "control": "Grouped searchable selector",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S06",
      "S07"
    ],
    "primaryScreen": "S06",
    "defaultState": "Effective preset envelope",
    "defaultValue": "Effective preset envelope",
    "scope": "Preset/task",
    "applyTiming": "At admission",
    "behavior": "Show inherited denies and exact MCP namespaces. A newly discovered tool remains unavailable until policy resolution allows it.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-06",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Role model and reasoning",
    "control": "Inherited model + supported overrides",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S23",
      "S06",
      "S07"
    ],
    "primaryScreen": "S23",
    "defaultState": "Inherit",
    "defaultValue": "Inherit",
    "scope": "Preset/task/child",
    "applyTiming": "Next admitted child/call",
    "behavior": "Show fallback and pricing boundary. Existing child model control is preserved; richer preset defaults need explicit configuration and adapter mapping.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 266
  },
  {
    "id": "A-07",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Delegation policy",
    "control": "Off / ask / permitted bounded delegation",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S23",
      "S06",
      "S07"
    ],
    "primaryScreen": "S23",
    "defaultState": "Effective runtime policy",
    "defaultValue": "Effective runtime policy",
    "scope": "Task",
    "applyTiming": "Before new child admission",
    "behavior": "Show max depth, parent budget share and allowed child roles. Disabling delegation prevents new children; it does not pretend to stop already-running children.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "A-08",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Child concurrency limit",
    "control": "Integer within served capacity",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S23",
      "S06",
      "S07"
    ],
    "primaryScreen": "S23",
    "defaultState": "Effective daemon limit",
    "defaultValue": "Effective daemon limit",
    "scope": "Task/operator",
    "applyTiming": "Next child admission",
    "behavior": "Display waiting versus active children. Saturation creates an owned bounded queue or a typed refusal; never unbounded spawning.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "A-09",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Child budget allocation",
    "control": "Shared cap plus optional child ceiling",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S23",
      "S50",
      "S06",
      "S07"
    ],
    "primaryScreen": "S23",
    "defaultState": "Shared parent envelope",
    "defaultValue": "Shared parent envelope",
    "scope": "Task/child",
    "applyTiming": "On grant acknowledgement",
    "behavior": "A child never gets an independent unlimited wallet. Preserve open reservations and uncertain spend when reducing an allowance.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "A-10",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Steer, pause and stop child",
    "control": "Contextual lifecycle actions",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S70",
      "S23",
      "S50",
      "S06"
    ],
    "primaryScreen": "S70",
    "defaultState": "No action until selected",
    "defaultValue": "No action until selected",
    "scope": "Child run",
    "applyTiming": "After durable control ack",
    "behavior": "Existing steer/model/agent controls are reused where served. Steer text retains the current 500-character bound; unsupported pause is disabled, not simulated.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "A-11",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Worktree/edit ownership",
    "control": "Read-only ownership + explicit assignment",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24",
      "S23",
      "S50",
      "S06",
      "S07",
      "S31"
    ],
    "primaryScreen": "S24",
    "defaultState": "Isolated candidate",
    "defaultValue": "Isolated candidate",
    "scope": "Task/child",
    "applyTiming": "Before edit ownership grant",
    "behavior": "Keep parallel agents from overwriting the same candidate. Shared-file contention shows owner, conflict and handoff; agents do not acquire arbitrary owner-checkout write access.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "A-12",
    "category": "Agent presets and delegation",
    "categoryCode": "A",
    "name": "Results and handoff schema",
    "control": "Typed summary/criteria/artifact fields",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S23",
      "S50",
      "S51",
      "S52",
      "S06",
      "S07"
    ],
    "primaryScreen": "S23",
    "defaultState": "Existing task evidence where served",
    "defaultValue": "Existing task evidence where served",
    "scope": "Child to parent",
    "applyTiming": "On child completion",
    "behavior": "Show what was requested, returned, verified and still missing. Child text saying done is not the parent task completion basis.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 267
  },
  {
    "id": "C-01",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Context meter",
    "control": "Used / reserved / available display",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S01",
      "S08",
      "S62"
    ],
    "primaryScreen": "S01",
    "defaultState": "Unavailable until authoritative read",
    "defaultValue": "Unavailable until authoritative read",
    "scope": "Call/session",
    "applyTiming": "On measured snapshot",
    "behavior": "Current projection contextUsage is null. Add a durable versioned read contract before showing real percentages; keep draft estimates explicitly labelled.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A11",
      "A12"
    ],
    "specPage": 268
  },
  {
    "id": "C-02",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Context breakdown",
    "control": "System / rules / tools / history / files / retrieval",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S62"
    ],
    "primaryScreen": "S08",
    "defaultState": "Measured or estimated categories",
    "defaultValue": "Measured or estimated categories",
    "scope": "Draft/call",
    "applyTiming": "On preview / after call",
    "behavior": "Categories must be mutually accountable and sum to the same basis. Output reserve and safety margin are not consumed input tokens.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A11",
      "A12"
    ],
    "specPage": 268
  },
  {
    "id": "C-03",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Mention picker",
    "control": "@file / folder / selection / diagnostics / terminal / diff / URL / past chat",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S37",
      "S09",
      "S20"
    ],
    "primaryScreen": "S37",
    "defaultState": "Nothing silently attached",
    "defaultValue": "Nothing silently attached",
    "scope": "Draft",
    "applyTiming": "On explicit selection",
    "behavior": "Show host URI, range, size and include/exclude state. A reference from another worktree resolves there, never against a global current directory.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A13"
    ],
    "specPage": 268
  },
  {
    "id": "C-04",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Current editor context",
    "control": "Off / selection / active file / explicit tabs",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S09"
    ],
    "primaryScreen": "S08",
    "defaultState": "Proposed: selection only with visible chip",
    "defaultValue": "Proposed: selection only with visible chip",
    "scope": "User/workspace default",
    "applyTiming": "At draft capture",
    "behavior": "Display every automatic inclusion. A tab title is not its contents; enabling all open files must show the added input and file scope.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A13"
    ],
    "specPage": 268
  },
  {
    "id": "C-05",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Attachment staging",
    "control": "Add / remove / retry per file",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S37",
      "S08",
      "S09"
    ],
    "primaryScreen": "S37",
    "defaultState": "Existing typed upload behavior",
    "defaultValue": "Existing typed upload behavior",
    "scope": "Session/draft",
    "applyTiming": "On upload and admission",
    "behavior": "Preserve current MIME/byte gates from the served exact model. Unsupported image/document attachments remain visible with a reason, never silently dropped.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A13",
      "A14"
    ],
    "specPage": 268
  },
  {
    "id": "C-06",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Clipboard and large paste",
    "control": "Text/image admission with preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S37",
      "S09"
    ],
    "primaryScreen": "S37",
    "defaultState": "Respect model capability",
    "defaultValue": "Respect model capability",
    "scope": "Draft",
    "applyTiming": "On paste / send",
    "behavior": "Collapse large text behind a preview while preserving full bounded content. Show an invalid image or oversize payload without clearing the rest of the draft.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A14"
    ],
    "specPage": 268
  },
  {
    "id": "C-07",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Pin/exclude context items",
    "control": "Per-item controls",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S09"
    ],
    "primaryScreen": "S08",
    "defaultState": "No implicit pinned content",
    "defaultValue": "No implicit pinned content",
    "scope": "Draft/task",
    "applyTiming": "Next captured request",
    "behavior": "Pinning keeps priority, not permission to exceed a hard window. Explain when an item cannot fit and require a deliberate trim or alternate model.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-08",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Ignore and protected patterns",
    "control": "Scope-aware ignore editor",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S09"
    ],
    "primaryScreen": "S08",
    "defaultState": "Existing tool/security rules remain authoritative",
    "defaultValue": "Existing tool/security rules remain authoritative",
    "scope": "Project/user policy",
    "applyTiming": "After policy ack",
    "behavior": "Separate search exclusions from security denies. A retrieval ignore file is not a guarantee that an authorized shell cannot read a file.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-09",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Retrieval strategy",
    "control": "Lexical / symbol / semantic / hybrid when served",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S28"
    ],
    "primaryScreen": "S08",
    "defaultState": "Effective available strategy",
    "defaultValue": "Effective available strategy",
    "scope": "Daemon/task",
    "applyTiming": "Next retrieval",
    "behavior": "No configured embedder means lexical/symbol fallback. Never label the index semantic merely because the UI contains that option.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-10",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Embedding provider/model",
    "control": "Served provider + model + embedding policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S04",
      "S08",
      "S28"
    ],
    "primaryScreen": "S04",
    "defaultState": "No embedder when absent",
    "defaultValue": "No embedder when absent",
    "scope": "Daemon/operator",
    "applyTiming": "After validated reload/reindex",
    "behavior": "Model changes invalidate incompatible vectors and show rebuild state. Credentials and data egress are disclosed before embedding a codebase.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-11",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Index coverage and freshness",
    "control": "Read-only coverage + reindex action if served",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S28"
    ],
    "primaryScreen": "S08",
    "defaultState": "Current index snapshot",
    "defaultValue": "Current index snapshot",
    "scope": "Workspace/index",
    "applyTiming": "On status refresh",
    "behavior": "Show indexed, ignored, stale and failed paths and commit basis. Existing status/coverage endpoints are reused; do not infer completion from a background spinner.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-12",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Context capacity guard",
    "control": "Preview and resolve overflow",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S10"
    ],
    "primaryScreen": "S08",
    "defaultState": "Block an over-cap request",
    "defaultValue": "Block an over-cap request",
    "scope": "Draft/call",
    "applyTiming": "Before admission",
    "behavior": "Offer remove, shorten, compact or choose a capable model. No hidden truncation of acceptance criteria, permissions or captured engineering obligations.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-13",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Source preview and provenance",
    "control": "Open included excerpt/source",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08",
      "S09"
    ],
    "primaryScreen": "S08",
    "defaultState": "Read-only",
    "defaultValue": "Read-only",
    "scope": "Context item",
    "applyTiming": "On selection",
    "behavior": "Show exact version/range and how it entered context. Treat tool output, fetched pages and repository instructions as data, not authority to change policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 269
  },
  {
    "id": "C-14",
    "category": "Context, attachments and retrieval",
    "categoryCode": "C",
    "name": "Context export",
    "control": "Redacted manifest and bounded excerpt export",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S08"
    ],
    "primaryScreen": "S08",
    "defaultState": "Exclude credentials and secret payloads",
    "defaultValue": "Exclude credentials and secret payloads",
    "scope": "User/session",
    "applyTiming": "On explicit export",
    "behavior": "Include IDs, hashes, basis and omission reasons. Mark estimates; do not export a guessed hidden prompt or private model chain of thought.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 270
  },
  {
    "id": "K-01",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Automatic compaction threshold",
    "control": "Validated usage fraction",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S10"
    ],
    "primaryScreen": "S10",
    "defaultState": "Daemon default 0.65",
    "defaultValue": "Daemon default 0.65",
    "scope": "Daemon; future task override",
    "applyTiming": "Next compaction decision",
    "behavior": "Use the daemon usage denominator. Show effective threshold and read source; do not reuse a graphical estimated context ratio as authoritative runtime usage.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "compact_at_usage"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 271
  },
  {
    "id": "K-02",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Compaction model",
    "control": "Served model choice",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S10"
    ],
    "primaryScreen": "S10",
    "defaultState": "Inherit when absent",
    "defaultValue": "Inherit when absent",
    "scope": "Daemon/operator",
    "applyTiming": "Next compaction",
    "behavior": "Show egress and cost implications and required capabilities. An unavailable compactor preserves the last valid state and reports failure.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "compaction_model"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 271
  },
  {
    "id": "K-03",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Compact now",
    "control": "Preview / compact / cancel before dispatch",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S10"
    ],
    "primaryScreen": "S10",
    "defaultState": "No automatic manual action",
    "defaultValue": "No automatic manual action",
    "scope": "Session",
    "applyTiming": "After durable operation ack",
    "behavior": "Requires an explicit new native operation contract. Show preserved facts, removed material and reduction; a failed minimum reduction cannot be labelled successful.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A15"
    ],
    "specPage": 271
  },
  {
    "id": "K-04",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Post-compaction inspection",
    "control": "Before/after manifest + task facts",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S10"
    ],
    "primaryScreen": "S10",
    "defaultState": "Latest successful compaction",
    "defaultValue": "Latest successful compaction",
    "scope": "Session",
    "applyTiming": "After completion",
    "behavior": "Retain immutable original history by reference, summary version and stale evidence flags. Re-expanding UI text does not restore model context automatically.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A15"
    ],
    "specPage": 271
  },
  {
    "id": "K-05",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Failure learning",
    "control": "On/off with provenance",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S12",
      "S28"
    ],
    "primaryScreen": "S12",
    "defaultState": "Production ON",
    "defaultValue": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose the real efficiency.failure_learning off-switch. Separate learned failure priors from personal memories and show disabled/fallback status.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "efficiency.failure_learning"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 271
  },
  {
    "id": "K-06",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Compressed tool evidence",
    "control": "On/off with archive links",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S28"
    ],
    "primaryScreen": "S28",
    "defaultState": "Production ON",
    "defaultValue": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.ccr. Disabling it does not remove bounded-output limits; show the durable full artifact and bounded model excerpt separately.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "efficiency.ccr"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 271
  },
  {
    "id": "K-07",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Typed handoff",
    "control": "On/off with retained-facts preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S28"
    ],
    "primaryScreen": "S28",
    "defaultState": "Production ON",
    "defaultValue": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.typed_handoff. Changing it never erases the task ledger; describe the current projection basis and loss of structured handoff benefits.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "efficiency.typed_handoff"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 272
  },
  {
    "id": "K-08",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Information-gain context",
    "control": "On/off",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S28"
    ],
    "primaryScreen": "S28",
    "defaultState": "Production ON",
    "defaultValue": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Canonical key semantic_context; information_gain is an alias, not another switch. Duplicate aliases must fail validation instead of silently winning.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "efficiency.semantic_context",
      "(alias",
      "information_gain)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 272
  },
  {
    "id": "K-09",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Rework-aware routing",
    "control": "On/off + outcome sample count",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S28"
    ],
    "primaryScreen": "S28",
    "defaultState": "Production ON",
    "defaultValue": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.rework_routing and distinguish prior declarations from verified outcome observations. No measured savings claim without a matching denominator.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "efficiency.rework_routing"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 272
  },
  {
    "id": "K-10",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Personal/project memory",
    "control": "Off / manual / controlled auto capture",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S12",
      "S66"
    ],
    "primaryScreen": "S12",
    "defaultState": "Proposed: manual only",
    "defaultValue": "Proposed: manual only",
    "scope": "User/project; separate stores",
    "applyTiming": "Next request after ack",
    "behavior": "New memory authority needs list/read/edit/delete, source, timestamp and retention. Do not reinterpret current failure-learning records as general auto-memory.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A16"
    ],
    "specPage": 272
  },
  {
    "id": "K-11",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Memory review and deletion",
    "control": "Diff before save; delete with scope",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S12",
      "S66"
    ],
    "primaryScreen": "S12",
    "defaultState": "No cross-project sharing",
    "defaultValue": "No cross-project sharing",
    "scope": "Memory record",
    "applyTiming": "After durable save/delete ack",
    "behavior": "Deleting a memory does not remove earlier prompt copies. Show downstream retention scope; prevent a project from selecting an arbitrary global memory location.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A16"
    ],
    "specPage": 272
  },
  {
    "id": "K-12",
    "category": "Compaction, memory and efficiency",
    "categoryCode": "K",
    "name": "Efficiency effect display",
    "control": "Observed counters and basis",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S53",
      "S27",
      "S28"
    ],
    "primaryScreen": "S53",
    "defaultState": "Unavailable when not recorded",
    "defaultValue": "Unavailable when not recorded",
    "scope": "Session/workspace",
    "applyTiming": "After measurement",
    "behavior": "Show compression reduction, cache observations and routing outcomes separately. No fabricated percent saved or zero-cost assumption for locally hosted inference.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 272
  },
  {
    "id": "I-01",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "User instructions",
    "control": "Markdown editor and preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "Effective daemon baseline",
    "defaultValue": "Effective daemon baseline",
    "scope": "User/daemon",
    "applyTiming": "Next request after ack",
    "behavior": "Keep unsaved text local. Show token estimate and source; do not claim a new native instructions API is already present.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "instructions"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-02",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Project instructions",
    "control": "AGENTS.md source view/editor",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "Discover only supported trusted sources",
    "defaultValue": "Discover only supported trusted sources",
    "scope": "Project",
    "applyTiming": "Next request after reload/ack",
    "behavior": "Display path, trust and active workspace. The repository guide is not proof that the runtime discovers every nested AGENTS.md file today.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "AGENTS.md"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-03",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Scoped rules",
    "control": "Rule editor: path scope + trigger",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "No new rule enabled silently",
    "defaultValue": "No new rule enabled silently",
    "scope": "Project/user",
    "applyTiming": "Next matching task",
    "behavior": "Validate glob and test against sample paths. Explain always/manual/path-matched activation and show the matching source in the request preview.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      ".faktor/rules",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-04",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Effective order",
    "control": "Read-only winning-value/merged-bundle inspector",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11",
      "S69"
    ],
    "primaryScreen": "S11",
    "defaultState": "Backend-served resolution",
    "defaultValue": "Backend-served resolution",
    "scope": "Task/session",
    "applyTiming": "On config snapshot",
    "behavior": "Separate enforced managed constraints from instruction text. Proposed ordinary preference order is task, project, user, daemon default; fields may restrict allowed scopes.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-05",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Rule enable and disable",
    "control": "Toggle with policy lock",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "Existing effective set",
    "defaultValue": "Existing effective set",
    "scope": "Rule/source",
    "applyTiming": "After acknowledged reload",
    "behavior": "Show inactive, invalid, disabled and locked states. Disabling one file must not disable an unrelated rule with a matching display name.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-06",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Prompt templates",
    "control": "Named template + variables",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11",
      "S17"
    ],
    "primaryScreen": "S11",
    "defaultState": "None",
    "defaultValue": "None",
    "scope": "User/project",
    "applyTiming": "On insert into draft",
    "behavior": "Template insertion does not execute tools or send a prompt. Required variables remain visibly unresolved until filled; preserve the original draft on cancel.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [
      "profiles/commands",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 273
  },
  {
    "id": "I-07",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Instruction conflict display",
    "control": "Explain conflicting sources",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "No silent conflict resolution in UI",
    "defaultValue": "No silent conflict resolution in UI",
    "scope": "Effective config",
    "applyTiming": "On validation",
    "behavior": "Show winner and reason, with links to editable sources. A locked policy cannot be overridden by appending a more forceful instruction.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 274
  },
  {
    "id": "I-08",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Import rules or presets",
    "control": "Preview, map and validate",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11",
      "S69"
    ],
    "primaryScreen": "S11",
    "defaultState": "No import executed automatically",
    "defaultValue": "No import executed automatically",
    "scope": "User/project",
    "applyTiming": "After explicit import save",
    "behavior": "Import supported semantics only. foreign-editor/Claude config files are not native Faktor formats; unsupported keys remain a visible migration report.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 274
  },
  {
    "id": "I-09",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "System prompt inspection",
    "control": "Redacted effective summary",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "No secret/system dump",
    "defaultValue": "No secret/system dump",
    "scope": "Task/call",
    "applyTiming": "Before admission when available",
    "behavior": "Show user-editable instructions and loaded-source manifest. Do not claim to reveal hidden provider instructions or protected model internals.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 274
  },
  {
    "id": "I-10",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Output style",
    "control": "Concise / explanatory / custom",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S11"
    ],
    "primaryScreen": "S11",
    "defaultState": "Project/user preference",
    "defaultValue": "Project/user preference",
    "scope": "User/preset",
    "applyTiming": "Next request",
    "behavior": "A communication style affects prose, not tool grants, verification policy or data egress. Show it in the effective prompt preview without occupying the main toolbar.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 274
  },
  {
    "id": "I-11",
    "category": "Instructions, rules and prompt composition",
    "categoryCode": "I",
    "name": "Instruction version binding",
    "control": "Read-only version/hash receipt",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S10",
      "S11",
      "S12"
    ],
    "primaryScreen": "S10",
    "defaultState": "Captured at admission",
    "defaultValue": "Captured at admission",
    "scope": "Task/call",
    "applyTiming": "At durable admission",
    "behavior": "Running work retains its captured bundle. Editing a file shows Next task or Restart required; it must not silently rewrite the instructions of an existing run.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 274
  },
  {
    "id": "Q-01",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Permission preset",
    "control": "Inspect / confirm edits / project auto / custom",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S01",
      "S13"
    ],
    "primaryScreen": "S36",
    "defaultState": "Proposed: inspect/read and ask before effects",
    "defaultValue": "Proposed: inspect/read and ask before effects",
    "scope": "Task/user within managed bounds",
    "applyTiming": "At admission; revocation acknowledged",
    "behavior": "Explain the exact tool categories affected. Avoid an ambiguous full-auto switch; full privilege is an explicit advanced policy, never inferred from Code mode.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "permission",
      "preset",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 275
  },
  {
    "id": "Q-02",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Per-tool rule",
    "control": "Allow / Ask / Deny",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Ask when no authorized rule applies",
    "defaultValue": "Ask when no authorized rule applies",
    "scope": "User/project/managed",
    "applyTiming": "After policy reload/ack",
    "behavior": "Persistent rules require a real policy store and dispatcher enforcement. Current permission-reply API alone cannot save an Always allow rule.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "permission-rules",
      "store",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 275
  },
  {
    "id": "Q-03",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "File scope matcher",
    "control": "Canonical roots + validated patterns",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Captured workspace/candidate",
    "defaultValue": "Captured workspace/candidate",
    "scope": "Rule/task",
    "applyTiming": "At each dispatch",
    "behavior": "Distinguish read, create, edit and delete. Normalize symlinks and worktree ownership server-side; a pretty glob is not a containment guarantee.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "sandbox.filesystem",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 275
  },
  {
    "id": "Q-04",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Command rule matcher",
    "control": "Structured executable/argument policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Ask for unclassified commands",
    "defaultValue": "Ask for unclassified commands",
    "scope": "Rule/task",
    "applyTiming": "At each dispatch",
    "behavior": "Preview the exact normalized command. Shell injection/composition and interpreter scripts must not be approved through a misleading prefix match.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "sandbox.shell"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 275
  },
  {
    "id": "Q-05",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Network and external effects",
    "control": "Destination + action policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Effective daemon deny/allow rules",
    "defaultValue": "Effective daemon deny/allow rules",
    "scope": "Operator/task",
    "applyTiming": "At dispatch",
    "behavior": "Read-only HTTP, authenticated API writes, uploads and messages have different effects. Show destination class and data boundary, not only a tool name.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "sandbox.network"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 275
  },
  {
    "id": "Q-06",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "One-shot approval card",
    "control": "Allow once / Deny + request details",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S22",
      "S23"
    ],
    "primaryScreen": "S14",
    "defaultState": "Pending current request",
    "defaultValue": "Pending current request",
    "scope": "Session/request generation",
    "applyTiming": "After exact reply ack",
    "behavior": "Preserve existing target/reason and stale/wrong-session handling. Disable duplicate reply buttons until a durable or typed outcome arrives.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A17"
    ],
    "specPage": 275
  },
  {
    "id": "Q-07",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Remember this decision",
    "control": "Preview persistent rule, then save",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S13"
    ],
    "primaryScreen": "S14",
    "defaultState": "Off",
    "defaultValue": "Off",
    "scope": "Named policy scope",
    "applyTiming": "Future matching dispatch only",
    "behavior": "A remembered rule cannot pre-authorize another pending ID. Show match scope and expiry; unavailable persistence leaves Allow once as the only approval option.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A18"
    ],
    "specPage": 276
  },
  {
    "id": "Q-08",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Rule evaluation preview",
    "control": "Test tool/action against policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Read-only simulator",
    "defaultValue": "Read-only simulator",
    "scope": "Rule draft",
    "applyTiming": "Before save",
    "behavior": "Show which normalized rule wins and why. Simulator output is advisory until the runtime returns the same versioned effective policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A18"
    ],
    "specPage": 276
  },
  {
    "id": "Q-09",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Expiry and revocation",
    "control": "Timed/session/task scope",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S25",
      "S13"
    ],
    "primaryScreen": "S14",
    "defaultState": "One request for one-shot approval",
    "defaultValue": "One request for one-shot approval",
    "scope": "Grant/rule",
    "applyTiming": "After revocation ack",
    "behavior": "Revoke prevents future effects and reports active operations separately. Do not imply an already-issued remote effect can be undone by revoking a local grant.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A17",
      "A28"
    ],
    "specPage": 276
  },
  {
    "id": "Q-10",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Sandbox status",
    "control": "Read-only mode and guarantees",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S57",
      "S13"
    ],
    "primaryScreen": "S57",
    "defaultState": "Actual served platform state",
    "defaultValue": "Actual served platform state",
    "scope": "Daemon host",
    "applyTiming": "On readiness/status refresh",
    "behavior": "Show filesystem, shell and network guarantees separately. Network-capable fallback is not a fully isolated shell. A failed setup stays a real blocker.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "sandbox.network_guarantee"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 276
  },
  {
    "id": "Q-11",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Sandbox setup choice",
    "control": "Native guidance/host-controlled flow",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S34",
      "S57",
      "S13"
    ],
    "primaryScreen": "S34",
    "defaultState": "Current fail-closed default",
    "defaultValue": "Current fail-closed default",
    "scope": "Machine/operator",
    "applyTiming": "After durable choice and restart if required",
    "behavior": "Reuse current faktor sandbox choose guidance until a real native setup API exists. Do not change shell security through an unacknowledged webview toggle.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "sandbox.shell"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 276
  },
  {
    "id": "Q-12",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Managed policy locks",
    "control": "Read-only lock + source + contact route",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S69",
      "S13"
    ],
    "primaryScreen": "S69",
    "defaultState": "No managed assumptions when disabled",
    "defaultValue": "No managed assumptions when disabled",
    "scope": "Organization/machine",
    "applyTiming": "On effective-config read",
    "behavior": "A locked setting cannot be relaxed by user/project defaults. Local coding remains available when the optional control plane is disabled.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "enterprise.settings"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 276
  },
  {
    "id": "Q-13",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Audit decision history",
    "control": "Bounded searchable decision log",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S13"
    ],
    "primaryScreen": "S14",
    "defaultState": "Only recorded decisions",
    "defaultValue": "Only recorded decisions",
    "scope": "Task/organization when enabled",
    "applyTiming": "After durable event",
    "behavior": "Show actor, scope, time, target and outcome; redact payload secrets. Permission requested, allowed, dispatched and effect confirmed remain different facts.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 276
  },
  {
    "id": "Q-14",
    "category": "Permissions, autonomy and sandbox",
    "categoryCode": "Q",
    "name": "Prompt-injection boundary",
    "control": "Source labels and policy refusal display",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S13"
    ],
    "primaryScreen": "S13",
    "defaultState": "Untrusted retrieved content",
    "defaultValue": "Untrusted retrieved content",
    "scope": "Tool/source item",
    "applyTiming": "On dispatch and review",
    "behavior": "A file, MCP result or webpage cannot instruct the UI to elevate authority. Show why an instruction was treated as data without blocking ordinary legitimate content.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 277
  },
  {
    "id": "T-01",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Tool catalog",
    "control": "Search grouped built-in/MCP tools",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "Current served tools only",
    "defaultValue": "Current served tools only",
    "scope": "Daemon/preset",
    "applyTiming": "On tool discovery",
    "behavior": "Show namespace, description, input schema, effect class and availability reason. An enabled tool can still be denied by active policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 278
  },
  {
    "id": "T-02",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Tool enablement",
    "control": "Enable/disable within preset envelope",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "Inherited effective set",
    "defaultValue": "Inherited effective set",
    "scope": "Preset/task",
    "applyTiming": "After toolset ack",
    "behavior": "Changing visibility never broadens grants. Disabled tools stop being offered on future calls; in-flight calls remain listed and separately cancellable.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 278
  },
  {
    "id": "T-03",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Add stdio MCP server",
    "control": "Name, executable and argument rows",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16"
    ],
    "primaryScreen": "S15",
    "defaultState": "No added server",
    "defaultValue": "No added server",
    "scope": "Machine/daemon trusted scope",
    "applyTiming": "After validation and supervised start",
    "behavior": "Match current bounds: at most 8 servers; name 128 bytes, command 512 bytes, 32 args of 512 bytes. Native CRUD is still required.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "mcp[]"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A21"
    ],
    "specPage": 278
  },
  {
    "id": "T-04",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Remote MCP transport",
    "control": "Streamable HTTP when supported",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16",
      "S68"
    ],
    "primaryScreen": "S15",
    "defaultState": "Unavailable in reviewed McpEntry config",
    "defaultValue": "Unavailable in reviewed McpEntry config",
    "scope": "Connection/operator",
    "applyTiming": "After new transport capability",
    "behavior": "Do not display current stdio config as an HTTP/OAuth implementation. Validate destination, authentication and reconnect semantics before enabling.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "mcp[].transport",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A22"
    ],
    "specPage": 278
  },
  {
    "id": "T-05",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "MCP environment and secrets",
    "control": "Explicit names/references and preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S16"
    ],
    "primaryScreen": "S16",
    "defaultState": "No inherited secret expansion",
    "defaultValue": "No inherited secret expansion",
    "scope": "Machine/server",
    "applyTiming": "Next supervised start",
    "behavior": "A new configuration/handoff contract must define allowed environment values. Never paste a bearer token into a shared project JSON field.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "mcp[].env",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 278
  },
  {
    "id": "T-06",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "MCP OAuth lifecycle",
    "control": "Connect / reconnect / revoke",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16",
      "S68"
    ],
    "primaryScreen": "S15",
    "defaultState": "Signed out",
    "defaultValue": "Signed out",
    "scope": "Machine/server account",
    "applyTiming": "After auth callback/revoke",
    "behavior": "Show account and server binding. Cancellation preserves the draft; failed auth does not become Connected. Current reviewed MCP config has no OAuth fields.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "mcp[].oauth",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A22"
    ],
    "specPage": 278
  },
  {
    "id": "T-07",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Server health",
    "control": "Connecting / ready / auth / failed / stopped",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "Observed state or unavailable",
    "defaultValue": "Observed state or unavailable",
    "scope": "Server instance",
    "applyTiming": "On heartbeat/observation",
    "behavior": "Show last success and bounded error detail. Installed does not mean connected, and connection does not certify tool behavior.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-08",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Per-MCP-tool permissions",
    "control": "Namespaced Allow/Ask/Deny",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "Inherited policy; ask for effects",
    "defaultValue": "Inherited policy; ask for effects",
    "scope": "Rule/preset",
    "applyTiming": "At dispatch",
    "behavior": "Use server instance plus stable tool name, not a display label. Newly discovered effectful tools do not inherit a careless wildcard from another server.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "permission-rules",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-09",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Tool argument inspector",
    "control": "Schema and normalized argument preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S15"
    ],
    "primaryScreen": "S22",
    "defaultState": "Collapsed inline summary",
    "defaultValue": "Collapsed inline summary",
    "scope": "Call",
    "applyTiming": "Before dispatch/after result",
    "behavior": "Redact secrets but show the exact target and material changes. Arguments changed after approval require a new authority decision.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-10",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Timeout and output bounds",
    "control": "Validated duration and byte caps",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16"
    ],
    "primaryScreen": "S15",
    "defaultState": "Effective runtime bounds",
    "defaultValue": "Effective runtime bounds",
    "scope": "Tool/server policy",
    "applyTiming": "Next operation",
    "behavior": "Show truncation and full artifact reference. A timeout marks whether an external effect is unknown; retry is not automatic for effectful tools.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "tool",
      "timeout/output",
      "caps"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-11",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Tool-definition loading",
    "control": "Deferred / pinned when supported",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "Effective runtime strategy",
    "defaultValue": "Effective runtime strategy",
    "scope": "Session/preset",
    "applyTiming": "Next context compile",
    "behavior": "Show definition/token cost when measurable. Installing many tools must not quietly consume the whole context window; unsupported deferred loading is explicit.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-12",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "MCP resources and prompts",
    "control": "Catalog + selected-item preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15"
    ],
    "primaryScreen": "S15",
    "defaultState": "None selected",
    "defaultValue": "None selected",
    "scope": "Session/draft",
    "applyTiming": "On explicit read/invoke",
    "behavior": "Resources, templates, prompts and tools have different lifecycles. Treat server-supplied instructions as attributed content, not unconditional policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-13",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Restart, logs and remove",
    "control": "Bounded logs and owned lifecycle actions",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16"
    ],
    "primaryScreen": "S15",
    "defaultState": "Keep current server until action",
    "defaultValue": "Keep current server until action",
    "scope": "Server instance",
    "applyTiming": "After owner ack",
    "behavior": "Restart reconciles pending calls and process ownership. Removing a config never turns an unknown remote effect into cancelled.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 279
  },
  {
    "id": "T-14",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Shared/project server trust",
    "control": "Review executable, origin and scope",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S16"
    ],
    "primaryScreen": "S16",
    "defaultState": "Not executable merely from repo config",
    "defaultValue": "Not executable merely from repo config",
    "scope": "Project/operator",
    "applyTiming": "After explicit trusted installation",
    "behavior": "A checked-out repository cannot silently run arbitrary stdio servers. Show executable provenance and which user/machine granted the start authority.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 280
  },
  {
    "id": "T-15",
    "category": "Tool catalog and MCP",
    "categoryCode": "T",
    "name": "Tool diagnostics/export",
    "control": "Redacted connection and schema bundle",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S15",
      "S16"
    ],
    "primaryScreen": "S15",
    "defaultState": "Exclude credential values",
    "defaultValue": "Exclude credential values",
    "scope": "User/server",
    "applyTiming": "On explicit export",
    "behavior": "Include versions, toolset hash and bounded failure context. Copying diagnostics must never export environment secrets or private tool-result payloads by default.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 280
  },
  {
    "id": "S-01",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Skill library",
    "control": "Search installed trusted skills",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "No fabricated marketplace inventory",
    "defaultValue": "No fabricated marketplace inventory",
    "scope": "User/project/preset",
    "applyTiming": "On discovery",
    "behavior": "Show source, version, description, invocation policy and required capabilities. Current source review does not establish a complete skill-management API.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-02",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Skill invocation policy",
    "control": "Manual / agent-selectable / disabled",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "Proposed: manual for effectful workflows",
    "defaultValue": "Proposed: manual for effectful workflows",
    "scope": "Skill/preset",
    "applyTiming": "Next invocation",
    "behavior": "Visibility and authority are distinct. A hidden skill must not remain silently invocable through a guessed command alias.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-03",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Skill details and preview",
    "control": "Manifest, files and declared capabilities",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "Read-only before installation",
    "defaultValue": "Read-only before installation",
    "scope": "Skill source",
    "applyTiming": "On open",
    "behavior": "Show exactly which instructions and scripts would load. Do not run setup or install dependencies merely to display a package.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-04",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Install/update/remove skill",
    "control": "Preview version diff, then apply",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "No silent project installation",
    "defaultValue": "No silent project installation",
    "scope": "User/project trusted store",
    "applyTiming": "After validated install ack",
    "behavior": "Keep prior known-good version on failed update. Removing a skill removes future discovery without pretending to erase earlier context copies.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-05",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Slash-command palette",
    "control": "Filterable commands with scope and effect",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "Documented available commands",
    "defaultValue": "Documented available commands",
    "scope": "Host/session",
    "applyTiming": "At invocation",
    "behavior": "Commands insert or act according to a visible label. Proposed names in this document are UX routes, not existing CLI commands.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-06",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Custom commands/templates",
    "control": "Editor and dry insertion preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17"
    ],
    "primaryScreen": "S17",
    "defaultState": "None",
    "defaultValue": "None",
    "scope": "User/project",
    "applyTiming": "On explicit invocation",
    "behavior": "Validate arguments and show whether a command is prompt-only or executable. Do not interpret pasted command-looking text as an automatic shell action.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 281
  },
  {
    "id": "S-07",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook event selector",
    "control": "Current 17-event enum",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18"
    ],
    "primaryScreen": "S18",
    "defaultState": "No new hook installed",
    "defaultValue": "No new hook installed",
    "scope": "Daemon/approved package",
    "applyTiming": "On config apply",
    "behavior": "Use exact native HookEvent names; do not copy Claude-specific events into Faktor config. Expose only events the running daemon supports.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "hooks[].events",
      "(env",
      "FAKTOR_HOOKS)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-08",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook executable and args",
    "control": "Executable reference + bounded argv",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18"
    ],
    "primaryScreen": "S18",
    "defaultState": "User supplied",
    "defaultValue": "User supplied",
    "scope": "Machine/trusted hook spec",
    "applyTiming": "After validation before spawn",
    "behavior": "Show executable host, working directory and version/source. A project hook cannot silently replace the machine runtime or expand its capability set.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "hooks[].command/args"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-09",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook match filter",
    "control": "Tool/event/path predicates when supported",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18"
    ],
    "primaryScreen": "S18",
    "defaultState": "Event set only unless extended",
    "defaultValue": "Event set only unless extended",
    "scope": "Hook spec",
    "applyTiming": "Next matching event",
    "behavior": "Preview matching events. A richer matcher is a new schema feature if the current HookSpec cannot express it.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-10",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook environment",
    "control": "Explicit allowlist + secret references",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18"
    ],
    "primaryScreen": "S18",
    "defaultState": "Native allowlist true",
    "defaultValue": "Native allowlist true",
    "scope": "Machine/hook spec",
    "applyTiming": "Next hook run",
    "behavior": "Show benign passthrough and explicit entries. Changing the switch must not dump the entire daemon environment into the child process.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "hooks[].env_allowlist"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-11",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook deadline/output caps",
    "control": "Bounded deadline/stdout/stderr",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18"
    ],
    "primaryScreen": "S18",
    "defaultState": "Effective native hook spec",
    "defaultValue": "Effective native hook spec",
    "scope": "Hook spec",
    "applyTiming": "Next run",
    "behavior": "On expiry, owned process tree is stopped and partial output cannot masquerade as an Allow verdict. Preserve a bounded audit excerpt.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "hooks[].deadline_ms/stdout_cap/stderr_cap"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-12",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook failure policy",
    "control": "FailClosed / FailOpen / Warn",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18",
      "S67"
    ],
    "primaryScreen": "S18",
    "defaultState": "Explicit per-hook policy",
    "defaultValue": "Explicit per-hook policy",
    "scope": "Operator/hook",
    "applyTiming": "Next run",
    "behavior": "Explain what an operation will do after failure. FailOpen is a policy choice, not an error swallowed by the UI; managed constraints still apply.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "hooks[].failure_policy"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-13",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Hook test and audit",
    "control": "Dry-test inputs + recorded verdicts",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S18",
      "S67"
    ],
    "primaryScreen": "S18",
    "defaultState": "No production event triggered by preview",
    "defaultValue": "No production event triggered by preview",
    "scope": "Hook/test session",
    "applyTiming": "After explicit test admission",
    "behavior": "Test under the real capability/deadline envelope. Show Allow/Deny/Warn/metadata changes, exit state and exact tested version.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "hooks[].test",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 282
  },
  {
    "id": "S-14",
    "category": "Skills, commands and hooks",
    "categoryCode": "S",
    "name": "Packaged customization trust",
    "control": "Origin, update and conflict review",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S17",
      "S18"
    ],
    "primaryScreen": "S17",
    "defaultState": "Untrusted until reviewed",
    "defaultValue": "Untrusted until reviewed",
    "scope": "User/project/operator",
    "applyTiming": "Before enabling executable content",
    "behavior": "Keep rule/skill/hook installation distinct from permission grants. Lock managed versions where policy requires and explain unsigned/unverified provenance honestly.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 283
  },
  {
    "id": "X-01",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Terminal process list",
    "control": "Session-owned processes and states",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S19"
    ],
    "primaryScreen": "S19",
    "defaultState": "Existing native process view",
    "defaultValue": "Existing native process view",
    "scope": "Session/task/agent",
    "applyTiming": "On event/snapshot",
    "behavior": "Show owner, working directory, age, process state and last output. A PID without a live owned handle is stale, not proof that the process is controllable.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27"
    ],
    "specPage": 284
  },
  {
    "id": "X-02",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Shell/profile and cwd",
    "control": "Served shell choice + workspace URI",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S19"
    ],
    "primaryScreen": "S19",
    "defaultState": "Daemon policy and candidate cwd",
    "defaultValue": "Daemon policy and candidate cwd",
    "scope": "Machine/task",
    "applyTiming": "Before spawn",
    "behavior": "Show which machine/container owns the shell. Do not use a global current directory or run in the owner checkout because the editor selection changed.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "sandbox.shell",
      "sandbox.home"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 284
  },
  {
    "id": "X-03",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Terminal output detail",
    "control": "Bounded head/tail + full artifact",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S19"
    ],
    "primaryScreen": "S19",
    "defaultState": "Current bounded service view",
    "defaultValue": "Current bounded service view",
    "scope": "Process/output artifact",
    "applyTiming": "On output page",
    "behavior": "Show truncation, byte count and cursor. Do not append unlimited logs to the chat DOM or model context.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 284
  },
  {
    "id": "X-04",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Run in background",
    "control": "Owned background lifecycle action",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S19"
    ],
    "primaryScreen": "S19",
    "defaultState": "Foreground unless configured/selected",
    "defaultValue": "Foreground unless configured/selected",
    "scope": "Process/task",
    "applyTiming": "After background ack",
    "behavior": "Clarify that closing a UI tab does not terminate an owned process. Show dependencies and whether the parent waits for the result.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 284
  },
  {
    "id": "X-05",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Stop, input and reconcile",
    "control": "Controls gated by owned handle",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S70",
      "S19"
    ],
    "primaryScreen": "S70",
    "defaultState": "No action without valid owner",
    "defaultValue": "No action without valid owner",
    "scope": "Process/session",
    "applyTiming": "After exact lifecycle ack",
    "behavior": "Keep terminal kill, run cancellation and session abort separate. Unknown process effect requires reconciliation, not a blind restart.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27"
    ],
    "specPage": 284
  },
  {
    "id": "X-06",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Browser connection",
    "control": "Adapter selector and auth/status",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S20"
    ],
    "primaryScreen": "S20",
    "defaultState": "Unavailable until advertised",
    "defaultValue": "Unavailable until advertised",
    "scope": "Machine/session",
    "applyTiming": "After explicit connect",
    "behavior": "Browser automation needs a real tool adapter and permission envelope. Show local/remote browser and session scope; no product promise from a mock preview.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 284
  },
  {
    "id": "X-07",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Preview URL and network scope",
    "control": "Validated URL + effective boundary",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S20"
    ],
    "primaryScreen": "S20",
    "defaultState": "No automatic arbitrary navigation",
    "defaultValue": "No automatic arbitrary navigation",
    "scope": "Task/browser",
    "applyTiming": "At navigation admission",
    "behavior": "Expose target host and credentials/account context. Do not equate a successfully opened page with verified application behavior.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-08",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Screenshot/artifact capture",
    "control": "Explicit capture and attachment",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S20"
    ],
    "primaryScreen": "S20",
    "defaultState": "No automatic cross-context sharing",
    "defaultValue": "No automatic cross-context sharing",
    "scope": "Task/artifact",
    "applyTiming": "After capture",
    "behavior": "Show origin and capture time; control whether screenshots enter model context. Keep secret-bearing pages out of shared exports by default.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-09",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Browser console/network observations",
    "control": "Bounded issue table",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S20"
    ],
    "primaryScreen": "S20",
    "defaultState": "Unavailable without adapter events",
    "defaultValue": "Unavailable without adapter events",
    "scope": "Task/browser",
    "applyTiming": "On observation",
    "behavior": "A clean console is one observation, not total correctness. Filter by severity while preserving the recorded underlying results.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-10",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Language-server diagnostics",
    "control": "Native Problems integration",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S09",
      "S20"
    ],
    "primaryScreen": "S09",
    "defaultState": "Current host diagnostics when included",
    "defaultValue": "Current host diagnostics when included",
    "scope": "Workspace/draft/task",
    "applyTiming": "On explicit context capture",
    "behavior": "Show file, range and diagnostic version. Stale diagnostics are not evidence that the current candidate fails or passes.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-11",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Inline code actions",
    "control": "Explain / fix / test / add to context",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S09",
      "S20"
    ],
    "primaryScreen": "S09",
    "defaultState": "No action automatically selected",
    "defaultValue": "No action automatically selected",
    "scope": "Editor selection",
    "applyTiming": "On explicit action",
    "behavior": "Capture file URI, range and version. Fix opens a bounded task draft; it does not silently edit or overwrite a running conversation.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-12",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "Terminal command approval display",
    "control": "Command, cwd, env names, bounds",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S19"
    ],
    "primaryScreen": "S14",
    "defaultState": "Effective permission policy",
    "defaultValue": "Effective permission policy",
    "scope": "Operation",
    "applyTiming": "Before execution",
    "behavior": "Keep secret values masked but material executable/arguments visible. Show sandbox guarantees and whether output will be attached to the model.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "sandbox.shell"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "X-13",
    "category": "Terminal, browser and language tooling",
    "categoryCode": "X",
    "name": "SSH, WSL and container binding",
    "control": "Read-only execution/context/vault map",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S72",
      "S20",
      "S31"
    ],
    "primaryScreen": "S72",
    "defaultState": "Actual active host binding",
    "defaultValue": "Actual active host binding",
    "scope": "Workspace/session",
    "applyTiming": "On binding/reconnect",
    "behavior": "A local URL resolves on the execution host. Enforce URI/path mapping and invalidate stale handles after a remote-host change without losing drafts.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A08",
      "A09",
      "A19",
      "A20",
      "A27",
      "A28",
      "A38",
      "A40"
    ],
    "specPage": 285
  },
  {
    "id": "B-01",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Run token cap",
    "control": "Exact non-negative integer",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S33",
      "S01",
      "S63",
      "S21"
    ],
    "primaryScreen": "S36",
    "defaultState": "Existing 0 = no limit",
    "defaultValue": "Existing 0 = no limit",
    "scope": "User default; per-run override",
    "applyTiming": "Captured at admission",
    "behavior": "Show No limit rather than raw zero in the normal UI. Reject decimals, negatives, overflow and malformed pasted numbers before posting; preserve the draft.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.budgetTokens",
      "max_tokens"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A30"
    ],
    "specPage": 286
  },
  {
    "id": "B-02",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Run money cap",
    "control": "Exact currency input plus advanced micro-USD",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S33",
      "S01",
      "S63",
      "S21"
    ],
    "primaryScreen": "S36",
    "defaultState": "Existing 0 = no limit",
    "defaultValue": "Existing 0 = no limit",
    "scope": "User default; per-run override",
    "applyTiming": "Captured at admission",
    "behavior": "Convert exactly using decimal arithmetic; never binary floating-point rounding. Example: USD 5.00 = 5000000 micro-USD, not 5 tokens or 5000000 dollars.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.budgetCostMicro",
      "max_cost_micro"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A30"
    ],
    "specPage": 286
  },
  {
    "id": "B-03",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Cap warning and enforcement",
    "control": "Soft warning + hard-refusal states",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "Effective backend policy",
    "defaultValue": "Effective backend policy",
    "scope": "Task",
    "applyTiming": "Before reservation/dispatch",
    "behavior": "Warn on configured threshold where supported. Unknown price cannot be assumed to fit; expose refusal or explicitly documented bounded pricing policy.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A30",
      "A31"
    ],
    "specPage": 286
  },
  {
    "id": "B-04",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Reservation display",
    "control": "Open / settled / refunded / uncertain",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S32",
      "S49",
      "S23",
      "S27",
      "S21"
    ],
    "primaryScreen": "S22",
    "defaultState": "Only durable observed values",
    "defaultValue": "Only durable observed values",
    "scope": "Task/session",
    "applyTiming": "After ledger observation",
    "behavior": "Open reservations are not final charges. Provider-reported and computed spend are different fields; uncertainty survives retries and cancellation.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "usage.durable.reservations"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 286
  },
  {
    "id": "B-05",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Budget revision",
    "control": "Explicit new allowance request",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "No silent active-run edit",
    "defaultValue": "No silent active-run edit",
    "scope": "Task/policy",
    "applyTiming": "After durable revision ack",
    "behavior": "A lower cap cannot erase settled spend or existing reservations. Show when a new ceiling blocks future work; revised draft defaults do not alter captured tasks.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "task",
      "budget",
      "revision",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 286
  },
  {
    "id": "B-06",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Provider retry attempts",
    "control": "Integer 1..8",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "Daemon 3 total attempts",
    "defaultValue": "Daemon 3 total attempts",
    "scope": "Daemon/operator",
    "applyTiming": "Next eligible request",
    "behavior": "Label total attempts, not retries, to avoid off-by-one cost expectations. State-aware replay must never repeat a request after durable effect/stream admission.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "retry.max_attempts"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 286
  },
  {
    "id": "B-07",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Backoff and jitter",
    "control": "Base/max durations and fraction",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "250 ms / 5000 ms / 0.2",
    "defaultValue": "250 ms / 5000 ms / 0.2",
    "scope": "Daemon/operator",
    "applyTiming": "Next eligible request",
    "behavior": "Base 1..60000 ms; max >= base and <=120000 ms; jitter 0..1. Show retry class and countdown, with Stop available.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "retry.base_delay_ms",
      "retry.max_delay_ms",
      "retry.jitter"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 287
  },
  {
    "id": "B-08",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Rate limit recovery",
    "control": "Retry-After/queue state + cancel",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "Effective retry policy",
    "defaultValue": "Effective retry policy",
    "scope": "Provider/operation",
    "applyTiming": "On typed limit",
    "behavior": "Distinguish provider rate limit, account quota and local queue saturation. Do not advertise a reset time that the provider did not supply.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 287
  },
  {
    "id": "B-09",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Loop/step/consecutive-failure limit",
    "control": "Served limits only",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21"
    ],
    "primaryScreen": "S21",
    "defaultState": "Effective daemon bound",
    "defaultValue": "Effective daemon bound",
    "scope": "Task/preset",
    "applyTiming": "At admission or policy ack",
    "behavior": "Expose actual loop guards without inventing an active max-turns knob. If not supported, show Planned and keep bounded backend behavior authoritative.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "agent",
      "loop/step",
      "bounds"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 287
  },
  {
    "id": "B-10",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Task/operation deadline",
    "control": "Explicit duration and cancellation policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S19",
      "S21"
    ],
    "primaryScreen": "S19",
    "defaultState": "Served owner deadline",
    "defaultValue": "Served owner deadline",
    "scope": "Task/operation",
    "applyTiming": "At admission",
    "behavior": "Show what expires: model call, tool, task or session. Timeout is not necessarily cancellation acknowledged or remote effect rolled back.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "verification.quick_max_s",
      "verification.unit_max_s"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 287
  },
  {
    "id": "B-11",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Connectivity and auth failure",
    "control": "Retry/connect/update actions by typed cause",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S21",
      "S30"
    ],
    "primaryScreen": "S21",
    "defaultState": "Keep coherent snapshot",
    "defaultValue": "Keep coherent snapshot",
    "scope": "Connection/session",
    "applyTiming": "On classified error",
    "behavior": "401, 429, timeout, unavailable provider and stale daemon need different actions. No generic Retry button that can duplicate an admitted effectful task.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A05",
      "A17",
      "A25",
      "A28",
      "A29",
      "A30",
      "A31",
      "A32"
    ],
    "specPage": 287
  },
  {
    "id": "B-12",
    "category": "Budgets, retries and reliability",
    "categoryCode": "B",
    "name": "Uncertain admission recovery",
    "control": "Receipt lookup / same-key retry when safe",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S39",
      "S14",
      "S21",
      "S30"
    ],
    "primaryScreen": "S39",
    "defaultState": "Original immutable submission",
    "defaultValue": "Original immutable submission",
    "scope": "Prompt/task",
    "applyTiming": "After durable reconciliation",
    "behavior": "Same UUID with same captured body may recover its receipt; changing model, attachments or budget creates a different request and must not reuse that key.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A28",
      "A29"
    ],
    "specPage": 287
  },
  {
    "id": "V-01",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Candidate changes view",
    "control": "File tree + unified/split native diff",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24",
      "S43",
      "S44"
    ],
    "primaryScreen": "S24",
    "defaultState": "Current immutable candidate",
    "defaultValue": "Current immutable candidate",
    "scope": "Task/candidate",
    "applyTiming": "On served change basis",
    "behavior": "Preserve current native diff integration. Display base/candidate hashes and scope; editor navigation must never compare against a different session’s files.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A15",
      "A29",
      "A33",
      "A34",
      "A35",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 288
  },
  {
    "id": "V-02",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Review comments and next task",
    "control": "Line comments + bounded follow-up",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24"
    ],
    "primaryScreen": "S24",
    "defaultState": "No edit from merely reading",
    "defaultValue": "No edit from merely reading",
    "scope": "Review/task draft",
    "applyTiming": "On explicit submit",
    "behavior": "Tie comments to exact file and diff version. Stale comments prompt rebase or clarification; a follow-up task gets its own captured contract.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A15",
      "A29",
      "A33",
      "A34",
      "A35",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 288
  },
  {
    "id": "V-03",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Hunk/file acceptance",
    "control": "Per-hunk review plus selected-change policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24",
      "S43"
    ],
    "primaryScreen": "S24",
    "defaultState": "Read-only review unless supported",
    "defaultValue": "Read-only review unless supported",
    "scope": "Candidate",
    "applyTiming": "After explicit supported action",
    "behavior": "Review marks do not integrate files. Partial landing needs new proof for the changed candidate; never treat old full-candidate evidence as proof for a cherry-picked subset.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A15",
      "A29",
      "A33",
      "A34",
      "A35",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 288
  },
  {
    "id": "V-04",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Checkpoint timeline",
    "control": "Stored before/after identity and coverage",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S25",
      "S49"
    ],
    "primaryScreen": "S25",
    "defaultState": "Only actual recorded checkpoints",
    "defaultValue": "Only actual recorded checkpoints",
    "scope": "Session/candidate",
    "applyTiming": "On checkpoint read",
    "behavior": "Hash-only rows may lack content for redo/diff. Show unavailable rather than promising a universal undo; no checkpoint is invented from a completed message.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A33"
    ],
    "specPage": 288
  },
  {
    "id": "V-05",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Restore preview",
    "control": "Code / conversation / both only when supported",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S25"
    ],
    "primaryScreen": "S25",
    "defaultState": "No destructive default selected",
    "defaultValue": "No destructive default selected",
    "scope": "Checkpoint/branch",
    "applyTiming": "Before a separate restore transaction",
    "behavior": "Show affected and untracked/uncovered files, external effects and unrelated user edits. Exact content preconditions must be enforced by the backend.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A33"
    ],
    "specPage": 288
  },
  {
    "id": "V-06",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Fork and redo",
    "control": "New branch lineage + supported redo",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S25"
    ],
    "primaryScreen": "S25",
    "defaultState": "Preserve current session",
    "defaultValue": "Preserve current session",
    "scope": "Session/checkpoint",
    "applyTiming": "After durable fork/restore ack",
    "behavior": "Conversation branching and file restoration are different. Missing after blobs disable redo; a fork must not erase subsequent work or duplicate remote side effects.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A33",
      "A34"
    ],
    "specPage": 288
  },
  {
    "id": "V-07",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Acceptance criteria and contract",
    "control": "Ordered criteria + constraints/invariants/risks/claims",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S33",
      "S38",
      "S01",
      "S40",
      "S41",
      "S42",
      "S24",
      "S45",
      "S46",
      "S32",
      "S51",
      "S10"
    ],
    "primaryScreen": "S36",
    "defaultState": "Existing per-task fields",
    "defaultValue": "Existing per-task fields",
    "scope": "Task draft",
    "applyTiming": "Captured at admission",
    "behavior": "Keep required status and claim scope visible. Show a versioned contract; editing the next draft cannot change criteria that an active candidate was evaluated against.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "completion_contract"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A35"
    ],
    "specPage": 289
  },
  {
    "id": "V-08",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Verification policy",
    "control": "Quick/unit/full and supported external provider",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S41",
      "S42",
      "S45",
      "S46",
      "S32"
    ],
    "primaryScreen": "S41",
    "defaultState": "60 s quick / 600 s unit / full background",
    "defaultValue": "60 s quick / 600 s unit / full background",
    "scope": "Daemon/task policy",
    "applyTiming": "After supported config reload",
    "behavior": "quick_max_s=0 disables service and leaves mutating work Unverified. Advanced/optional provider availability is explicit; native checks remain independently useful.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "verification.quick_max_s",
      "verification.unit_max_s",
      "verification.full_as_background"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A15",
      "A29",
      "A33",
      "A34",
      "A35",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 289
  },
  {
    "id": "V-09",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Completion basis",
    "control": "Acceptance / claims / matching landing / finish",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S41",
      "S24",
      "S45",
      "S47",
      "S32",
      "S65",
      "S26",
      "S27"
    ],
    "primaryScreen": "S41",
    "defaultState": "Pending until all required facts exist",
    "defaultValue": "Pending until all required facts exist",
    "scope": "Task/candidate",
    "applyTiming": "On durable proof/landing",
    "behavior": "Separate every boundary; missing required result stays pending. Session Completed or an empty passing required set cannot independently establish verified engineering success.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "completion",
      "proof"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A35",
      "A36"
    ],
    "specPage": 289
  },
  {
    "id": "V-10",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Finish level",
    "control": "Local / commit / push / PR, hierarchical",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S36",
      "S33",
      "S01",
      "S41",
      "S48",
      "S32",
      "S65"
    ],
    "primaryScreen": "S36",
    "defaultState": "Local",
    "defaultValue": "Local",
    "scope": "Task draft",
    "applyTiming": "Captured at admission",
    "behavior": "Both current hosts now expose matching four levels. State implied earlier actions; selecting PR requires actual SCM configuration and does not bypass verification.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "completion",
      "level"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A35"
    ],
    "specPage": 289
  },
  {
    "id": "V-11",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "SCM target and privacy",
    "control": "Remote/base/branch/draft PR summary",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24",
      "S48",
      "S32"
    ],
    "primaryScreen": "S24",
    "defaultState": "Effective configured target",
    "defaultValue": "Effective configured target",
    "scope": "Task/operator",
    "applyTiming": "Before captured finish",
    "behavior": "Show repo, target branch and access scope. Missing canonical SCM is a blocker; a successful arbitrary shell command is not a native PR receipt.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "completion.scm",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A15",
      "A29",
      "A33",
      "A34",
      "A35",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 289
  },
  {
    "id": "V-12",
    "category": "Changes, checkpoints, verification and finish",
    "categoryCode": "V",
    "name": "Conflict and manual-edit recovery",
    "control": "Inspect / rebase new candidate / preserve edits",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S24",
      "S47",
      "S44",
      "S32",
      "S25"
    ],
    "primaryScreen": "S24",
    "defaultState": "No overwrite of unrelated work",
    "defaultValue": "No overwrite of unrelated work",
    "scope": "Candidate/owner checkout",
    "applyTiming": "On compare-and-swap failure",
    "behavior": "Show changed basis and new verification obligations. Restoring or editing invalidates stale proof; retain the previous verified artifact as historical evidence.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "expected_current_hash"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A34"
    ],
    "specPage": 289
  },
  {
    "id": "D-01",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Conversation identity and selector",
    "control": "Title + workspace/worktree + active run",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S26",
      "S61",
      "S72",
      "S31"
    ],
    "primaryScreen": "S26",
    "defaultState": "Existing durable conversation",
    "defaultValue": "Existing durable conversation",
    "scope": "Session/host",
    "applyTiming": "On deliberate switch",
    "behavior": "Preserve per-session draft, scroll anchor and tool view. Late callbacks from the previous session cannot overwrite the newly selected session.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-02",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Title/history search",
    "control": "Explicit Search conversation titles",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S26",
      "S61"
    ],
    "primaryScreen": "S26",
    "defaultState": "Existing title filter",
    "defaultValue": "Existing title filter",
    "scope": "Selected project/session list",
    "applyTiming": "On query",
    "behavior": "Show loaded/list scope and paging. Do not claim full-text conversation search from a title-only filter. Distinguish no matches from history unavailable.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-03",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Open-transcript search",
    "control": "Find with next/previous, case and regex options",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S26",
      "S71"
    ],
    "primaryScreen": "S26",
    "defaultState": "Off",
    "defaultValue": "Off",
    "scope": "Open conversation",
    "applyTiming": "On host action",
    "behavior": "Search the full paged transcript, including not-yet-rendered messages. A virtualized screen subset is not the full search universe.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-04",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Rename, pin and archive",
    "control": "Durable session metadata actions",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S26"
    ],
    "primaryScreen": "S26",
    "defaultState": "Current state",
    "defaultValue": "Current state",
    "scope": "Session",
    "applyTiming": "After metadata ack",
    "behavior": "Pinning may be host-local if labelled. Rename/archive needs real metadata support; never just change the displayed text and lose it on refresh.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "session",
      "metadata",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-05",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Queue and steer main run",
    "control": "Visible queue with edit/remove/next-step timing",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S38",
      "S39",
      "S01",
      "S22",
      "S59"
    ],
    "primaryScreen": "S38",
    "defaultState": "Do not interrupt implicitly",
    "defaultValue": "Do not interrupt implicitly",
    "scope": "Session/prompt queue",
    "applyTiming": "After admission or queue edit ack",
    "behavior": "Use existing prompt admission/queue semantics where available. Separate send next, interrupt then send, and a draft not yet admitted; show queue ownership and cap.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "prompt",
      "queue"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-06",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Fork, export and delete",
    "control": "Scoped lifecycle/export actions",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S25",
      "S26"
    ],
    "primaryScreen": "S25",
    "defaultState": "No deletion selected",
    "defaultValue": "No deletion selected",
    "scope": "Session",
    "applyTiming": "After appropriate durable/host action",
    "behavior": "Export a bounded redacted transcript plus metadata. Delete states what records, artifacts and forks remain; a running task needs a real stop/retain decision.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "session",
      "export/delete",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 290
  },
  {
    "id": "D-07",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Live tool timeline",
    "control": "Running/queued/waiting/failed/unknown parts",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S01",
      "S40",
      "S22",
      "S70",
      "S49",
      "S52",
      "S27",
      "S19"
    ],
    "primaryScreen": "S01",
    "defaultState": "Compact tool summaries",
    "defaultValue": "Compact tool summaries",
    "scope": "Session/operation",
    "applyTiming": "On durable events",
    "behavior": "Show model calls, tool results, approvals, cost and changed-file badges. Preserve original timestamps and output artifacts; do not invent a percent-complete bar.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-08",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Focus/verbose display",
    "control": "Compact / balanced / detailed",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S58"
    ],
    "primaryScreen": "S22",
    "defaultState": "Proposed: balanced",
    "defaultValue": "Proposed: balanced",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Collapsing tool detail does not hide pending approval, failures, missing proof or current task checklist. Expansion is local UI state, not new model context.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-09",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Theme, zoom and density",
    "control": "Inherit host; optional density",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29",
      "S58"
    ],
    "primaryScreen": "S29",
    "defaultState": "Host theme/font/zoom",
    "defaultValue": "Host theme/font/zoom",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Respect VS Code tokens and JetBrains UIManager/JBUI metrics. Marketing fonts stay on document/website surfaces, not IDE reading controls.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [
      "host",
      "theme/zoom",
      "density",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-10",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Composer send behavior",
    "control": "Enter inserts newline; host send action",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29"
    ],
    "primaryScreen": "S29",
    "defaultState": "Current Ctrl/Cmd+Enter pattern",
    "defaultValue": "Current Ctrl/Cmd+Enter pattern",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Offer discoverable send settings without stealing standard Tab/Shift+Tab focus. IME composition and paste must never accidentally admit a task.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [
      "send",
      "keybinding"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-11",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Keymap/commands and discovery",
    "control": "Native command registration and searchable help",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29",
      "S64"
    ],
    "primaryScreen": "S29",
    "defaultState": "Host keymap",
    "defaultValue": "Host keymap",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Show shortcuts in tooltips and menus; allow remap. Proposed /model, /context and /permissions are aliases only after implemented, not current command claims.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [
      "package.json",
      "commands/keybindings"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-12",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Notifications and attention",
    "control": "Permission/failure/completion categories",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S14",
      "S22"
    ],
    "primaryScreen": "S14",
    "defaultState": "Local notices; no unsolicited external sends",
    "defaultValue": "Local notices; no unsolicited external sends",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Never cover the editor for routine progress. Pending questions name the child/session; alerts remain available to screen readers without announcing every streamed token.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-13",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Performance and transcript window",
    "control": "Paged history, anchored viewport, bounded buffers",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S22",
      "S26",
      "S30",
      "S56"
    ],
    "primaryScreen": "S22",
    "defaultState": "Existing host windowing/coalescing",
    "defaultValue": "Existing host windowing/coalescing",
    "scope": "Host/session",
    "applyTiming": "On render/page",
    "behavior": "Preserve follow-tail versus manual scroll. Recover offscreen matches through paging; do not drop durable events to make animation look smoother.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A40"
    ],
    "specPage": 291
  },
  {
    "id": "D-14",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Accessibility and reduced motion",
    "control": "Native semantics + audit/help view",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29",
      "S58"
    ],
    "primaryScreen": "S29",
    "defaultState": "Host accessibility preferences",
    "defaultValue": "Host accessibility preferences",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Keyboard reaches every control; visible focus and labels survive 240 px panels and 200% text. Use names/shapes with color, editable plain-text tables and deliberate live-region summaries.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "H",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A39"
    ],
    "specPage": 292
  },
  {
    "id": "D-15",
    "category": "Sessions, displays and accessibility",
    "categoryCode": "D",
    "name": "Parallel conversations and worktrees",
    "control": "Session manager with owned tabs and active-run badges",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S26",
      "S72",
      "S31"
    ],
    "primaryScreen": "S26",
    "defaultState": "Existing selected conversation; bounded proposed parallelism",
    "defaultValue": "Existing selected conversation; bounded proposed parallelism",
    "scope": "Host/session/worktree",
    "applyTiming": "After supported admission or switch ack",
    "behavior": "Show which worktree, daemon and run owns each tab. A tab switch does not stop work; creating another active task requires explicit capacity and budget admission. Never share drafts or changed-file identities across worktrees.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 292
  },
  {
    "id": "O-01",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Effective-settings search",
    "control": "Search all settings and explain winner",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29",
      "S64",
      "S60"
    ],
    "primaryScreen": "S29",
    "defaultState": "All effective readable values",
    "defaultValue": "All effective readable values",
    "scope": "Allowed scopes only",
    "applyTiming": "On snapshot refresh",
    "behavior": "Each row shows label, source, scope, timing, reset and lock. Hide secrets; compare changes before save and keep invalid edits without partial persistence.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "effective",
      "settings",
      "search",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-02",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Local/hosted data boundary",
    "control": "Endpoint/egress summary and policy",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S31"
    ],
    "primaryScreen": "S31",
    "defaultState": "Actual configured route",
    "defaultValue": "Actual configured route",
    "scope": "Task/connection",
    "applyTiming": "Before capture and route change",
    "behavior": "Local to the service is not automatically local to the user. A hosted fallback must show its data boundary; turning cloud features off does not disable legitimate local work.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "route/egress",
      "policy"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-03",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Diagnostic export",
    "control": "Preview and redacted bounded bundle",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S30"
    ],
    "primaryScreen": "S30",
    "defaultState": "Local save only",
    "defaultValue": "Local save only",
    "scope": "User/session",
    "applyTiming": "On explicit action",
    "behavior": "Include versions, route/capability state, ownership and bounded errors; exclude credential values, source payloads and private screenshots unless deliberately selected.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "diagnostic",
      "export",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-04",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Runtime location and trust",
    "control": "Version/path/launcher/checksum details",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S34",
      "S35",
      "S29",
      "S30",
      "S56"
    ],
    "primaryScreen": "S34",
    "defaultState": "Current discovery policy",
    "defaultValue": "Current discovery policy",
    "scope": "Machine/application only",
    "applyTiming": "At explicit service start",
    "behavior": "Project settings cannot change the executable path. Recorded checksum match is not signature verification; mismatch refuses start and unrecorded paths need exact-path confirmation.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.binaryPath",
      "faktor.dataDir",
      "faktor.installRoot"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-05",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Start/stop/update lifecycle",
    "control": "Explicit actions and readiness stages",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S34",
      "S35",
      "S30"
    ],
    "primaryScreen": "S34",
    "defaultState": "No start from mere view opening",
    "defaultValue": "No start from mere view opening",
    "scope": "Machine/daemon",
    "applyTiming": "After lifecycle ack",
    "behavior": "Show starting, ready, incompatible, stopped and refused separately. Signed updater availability is conditional; an update action must preserve active-run ownership and rollback state.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "updater.*"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-06",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Usage/pricing/account view",
    "control": "Observed counts and quota basis",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S32",
      "S53",
      "S54",
      "S27"
    ],
    "primaryScreen": "S32",
    "defaultState": "Unavailable rather than zero",
    "defaultValue": "Unavailable rather than zero",
    "scope": "Local/session/org when enabled",
    "applyTiming": "On served usage/account read",
    "behavior": "Separate API charges, local operating estimates, subscription quota and reservations. Provider quotas are only shown when supplied; no fake balance on a local model.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "billing.*"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 293
  },
  {
    "id": "O-07",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Optional control-plane sign-in",
    "control": "Endpoint + organization + scoped secret-store flow",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S54",
      "S29",
      "S55",
      "S30",
      "S31"
    ],
    "primaryScreen": "S54",
    "defaultState": "Disabled/empty endpoint",
    "defaultValue": "Disabled/empty endpoint",
    "scope": "Machine/user account",
    "applyTiming": "After verified auth/revoke",
    "behavior": "VS Code has a guided path; JetBrains currently includes manual credential storage. Removing a local stored credential is distinct from revoking a remote session.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "E",
    "configKeys": [
      "faktor.controlPlaneEndpoint",
      "faktor.controlPlaneOrganization"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "exists",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-08",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Managed configuration",
    "control": "Lock/provenance inspector and allowed overrides",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29"
    ],
    "primaryScreen": "S29",
    "defaultState": "Disabled when enterprise not enabled",
    "defaultValue": "Disabled when enterprise not enabled",
    "scope": "Organization/operator",
    "applyTiming": "After effective policy ack",
    "behavior": "Reuse current enterprise effective-config concepts only when enabled. Do not imply arbitrary local preference keys are already enforced by that service.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "enterprise.*"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-09",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Retention and cleanup",
    "control": "Bounded policy and dry-run list",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S12",
      "S30"
    ],
    "primaryScreen": "S12",
    "defaultState": "Current stored retention policy",
    "defaultValue": "Current stored retention policy",
    "scope": "User/operator",
    "applyTiming": "After acknowledged policy/job",
    "behavior": "Show sessions, snapshots, artifacts, logs and learned facts separately. Never delete active-owned records or silently equate hiding chat with deleting retained files.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "retention",
      "policy",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-10",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Telemetry and feedback",
    "control": "Explicit categories and payload preview",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29"
    ],
    "primaryScreen": "S29",
    "defaultState": "Proposed: no content telemetry by default",
    "defaultValue": "Proposed: no content telemetry by default",
    "scope": "User/managed policy",
    "applyTiming": "After acknowledged choice",
    "behavior": "Local UX counters and uploaded content are different. Opt-in/out must show endpoint and scope; no unsupported compliance or confidentiality badge.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "telemetry",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "missing",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-11",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Worker placement",
    "control": "Local / served remote worker",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S31"
    ],
    "primaryScreen": "S31",
    "defaultState": "Local unless configured",
    "defaultValue": "Local unless configured",
    "scope": "Task/operator",
    "applyTiming": "At placement admission",
    "behavior": "Workers are optional. Show job generation, machine, lease and egress boundary; the IDE cannot invent a remote capacity or silently move local work to cloud.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "R",
    "configKeys": [
      "workers.*"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-12",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Import/export configuration",
    "control": "Redacted schema-validated bundle",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29"
    ],
    "primaryScreen": "S29",
    "defaultState": "No secrets or executable auto-start",
    "defaultValue": "No secrets or executable auto-start",
    "scope": "User/operator",
    "applyTiming": "After preview and apply",
    "behavior": "Report unsupported keys, policy conflicts, required restart and missing secret references. Never import machine executable paths from an untrusted project.",
    "states": {
      "empty": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
      "error": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "config",
      "import/export",
      "(proposed)"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  },
  {
    "id": "O-13",
    "category": "Privacy, diagnostics, optional cloud and distribution",
    "categoryCode": "O",
    "name": "Feature/capability status",
    "control": "Available / disabled / unsupported / needs development",
    "hosts": [
      "vscode",
      "jetbrains"
    ],
    "hostApplicability": "both",
    "screens": [
      "S29",
      "S30"
    ],
    "primaryScreen": "S29",
    "defaultState": "Actual daemon and host support",
    "defaultValue": "Actual daemon and host support",
    "scope": "Daemon/host",
    "applyTiming": "On version/capability handshake",
    "behavior": "Keep unavailable controls explainable and relevant, not a sea of inert toggles. The design catalog is complete; shipped menus include only backed capabilities.",
    "states": {
      "empty": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
      "error": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
      "disabled": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
      "loading": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
      "stale": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
      "pending": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
      "source": "Per-control empty/error/disabled copy from the Rev8 catalog; loading/stale/pending are the global state rules, not control-specific copy."
    },
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "loadingState": "Bounded and cancellable; keep the last coherent data; no fabricated progress (global state contract, p.222).",
    "staleState": "Keep the last coherent snapshot with age; disable admission/apply until reconciled (global state contract, p.222).",
    "pendingState": "Pending/unknown is a named state; suppress duplicate effects and reconcile before replay (global state contract, p.222).",
    "classification": "N",
    "configKeys": [
      "version/capability",
      "handshake"
    ],
    "endpoints": [],
    "contractGap": "",
    "currentStatus": "partial",
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 294
  }
];

export const E8_WORKFLOWS: readonly Edition8Workflow[] = [
  {
    "id": "W01",
    "title": "Connect and validate a custom provider",
    "screens": [
      "S03",
      "S02",
      "S05"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "provider/model pair and supported normalized tuning are captured only after acknowledgement (p.183).",
    "forbiddenFakeStates": "Fake \"Connected\" from config save; fake probe success; fake capability authority from a model marketing name; fake effective state from a green toast; secret values in project files."
  },
  {
    "id": "W02",
    "title": "Set up a local model without false locality",
    "screens": [
      "S04",
      "S31",
      "S08"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "a ready local route is explicit and observed usage remains unavailable until a durable read exists (p.183).",
    "forbiddenFakeStates": "\"Local\" inferred from a URL on the laptop; observed allocation fabricated from the model name; a private-LAN route shown as enabled; semantic index claimed without an embedder."
  },
  {
    "id": "W03",
    "title": "Tune a model and change it mid-session",
    "screens": [
      "S02",
      "S05",
      "S01"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "each executed call records actual route and effective parameters, separately from current draft preference (p.183).",
    "forbiddenFakeStates": "Showing the picker's model as the executed model; rewriting an in-flight call; treating a reasoning enum as a writable API; silently dropping an unsupported parameter."
  },
  {
    "id": "W04",
    "title": "Move from Ask to Plan to Code",
    "screens": [
      "S06",
      "S07",
      "S01"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "a new Code admission binds the approved plan version and execution envelope (p.183).",
    "forbiddenFakeStates": "Calling a plain prompt \"read-only\"; approving a plan and silently broadening authority; treating a preset label as a permission grant."
  },
  {
    "id": "W05",
    "title": "Assemble context and resolve overflow",
    "screens": [
      "S09",
      "S08",
      "S10"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "the admitted request has a versioned source manifest and valid capacity calculation (p.184).",
    "forbiddenFakeStates": "A filename or tab title shown as attached content; a composed context number labelled measured; silent omission of a failed attachment; hidden truncation of criteria."
  },
  {
    "id": "W06",
    "title": "Compact and inspect continuity",
    "screens": [
      "S10",
      "S08",
      "S32"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "required contract, candidate identity and unresolved obligations remain reachable and active (p.184).",
    "forbiddenFakeStates": "A compacted transcript view shown as runtime context reduction; a 1% summary accepted as success; deletion implied by compaction; measured reduction fabricated from an estimate."
  },
  {
    "id": "W07",
    "title": "Approve once or create a narrow rule",
    "screens": [
      "S14",
      "S13",
      "S22"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "one exact approval resolves once; a persisted rule has named scope, version and expiry (p.184).",
    "forbiddenFakeStates": "Persisting Allow once automatically; a green \"Allowed\" toast without dispatch; a rule shown active without a policy store; duplicate reply generating a second effect."
  },
  {
    "id": "W08",
    "title": "Add MCP with trust and recover auth",
    "screens": [
      "S16",
      "S15",
      "S13"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "only discovered, healthy, enabled and authorized tools are offered to the correct task (p.184).",
    "forbiddenFakeStates": "Config save shown as Connected; Ready without handshake; remote/OAuth shown from stdio config; repository content starting a server; disconnect treated as Cancelled."
  },
  {
    "id": "W09",
    "title": "Install and invoke a skill safely",
    "screens": [
      "S17",
      "S11",
      "S13"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "versioned instructions are attributed and execution still uses effective grants (p.185).",
    "forbiddenFakeStates": "Viewing a package executing setup; installation shown as authorization; hidden skill still invocable; instruction text widening grants."
  },
  {
    "id": "W10",
    "title": "Author and test a lifecycle hook",
    "screens": [
      "S18",
      "S30",
      "S22"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "a hook run cites exact spec version, event, envelope and bounded audit artifact (p.185).",
    "forbiddenFakeStates": "Partial output after timeout shown as Allow; test run emitting a real production lifecycle event; malformed verdict treated as pass; fail-open hidden."
  },
  {
    "id": "W11",
    "title": "Delegate and steer a bounded child",
    "screens": [
      "S07",
      "S23",
      "S14"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "shared budget and ownership hold; a child saying done does not establish parent verification (p.185).",
    "forbiddenFakeStates": "Child \"done\" shown as parent verified; unbounded child wallets; stale controls acting on a new generation; steer silently truncated."
  },
  {
    "id": "W12",
    "title": "Queue, background and interrupt work",
    "screens": [
      "S22",
      "S19",
      "S26"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "each queue item/process/control retains owner, generation and durable result (p.185).",
    "forbiddenFakeStates": "Closed tab shown as cancellation; lost stream shown as idle; stop shown as confirmed remote rollback; queue item silently discarded."
  },
  {
    "id": "W13",
    "title": "Handle rate limit, cap and unknown spend",
    "screens": [
      "S21",
      "S22",
      "S27"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "the final ledger retains every eligible attempted charge, including failed/cancelled work (p.186).",
    "forbiddenFakeStates": "Guessed reset time; zero for unknown price; cap decrease shown as deleting spend; cancelled work omitted from the eligible numerator."
  },
  {
    "id": "W14",
    "title": "Review, restore or fork without overwriting edits",
    "screens": [
      "S24",
      "S25",
      "S26"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "a changed candidate invalidates stale proof; historical verified artifacts remain historical (p.186).",
    "forbiddenFakeStates": "Universal undo claimed from hashes; restore silently overwriting user edits; fork shown as file rollback; old proof reused for a new tree."
  },
  {
    "id": "W15",
    "title": "Finish a change with complete proof",
    "screens": [
      "S01",
      "S24",
      "S32"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "required acceptance, claims, landing equality and requested finish all have explicit matching facts (p.186).",
    "forbiddenFakeStates": "Green finish from chat completion; proof reused across candidate identities; PR created without SCM; empty required set treated as success."
  },
  {
    "id": "W16",
    "title": "Recover a remote host or runtime change",
    "screens": [
      "S31",
      "S30",
      "S26"
    ],
    "controls": [],
    "cells": [
      "pointer",
      "keyboard",
      "screen_reader",
      "compact_zoom",
      "recovery"
    ],
    "completeWhen": "all callbacks and controls are fenced to the new binding; unknown effects stay visible (p.186).",
    "forbiddenFakeStates": "Old handle controlling a new host; local URL shown as daemon loopback; copied credentials; replaying an old effectful request on a new host."
  }
];

export const E8_SCREEN_BY_ID: ReadonlyMap<string, Edition8Screen> = new Map(
  E8_SCREENS.map((screen) => [screen.id, screen]),
);

export const E8_CONTROL_BY_ID: ReadonlyMap<string, Edition8Control> = new Map(
  E8_CONTROLS.map((control) => [control.id, control]),
);
