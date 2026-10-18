// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: docs/specs/edition5/{controls.json,screens.md}
// Regenerate: node scripts/generate-edition5.mjs --write
//
// This registry is the frozen Edition 5 contract data. Behavior lives in
// src/edition5/*; the registry only answers "what does the spec say".

export interface Edition5Control {
  readonly id: string;
  readonly category: string;
  readonly categoryCode: string;
  readonly name: string;
  readonly control: string;
  readonly hosts: readonly string[];
  readonly hostApplicability: string;
  readonly screens: readonly string[];
  readonly catalogScreen: string;
  readonly defaultState: string;
  readonly scope: string;
  readonly applyTiming: string;
  readonly behavior: string;
  readonly emptyState: string;
  readonly errorState: string;
  readonly disabledState: string;
  readonly classification: string;
  readonly buildPackages: readonly string[];
  readonly configKeys: readonly string[];
  readonly runtimeRequired: readonly string[];
  readonly runtimeEndpoints: readonly string[];
  readonly featureRefs: readonly string[];
  readonly acceptanceIds: readonly string[];
  readonly specPage: number;
  readonly currentStatus: string;
  readonly statusNote: string;
}

export interface Edition5Screen {
  readonly id: string;
  readonly title: string;
  readonly purpose: string;
  readonly action: string;
  readonly recovery: string;
  readonly controls: readonly string[];
  readonly layoutRegions: string;
  readonly vscodeVariant: string;
  readonly jetbrainsVariant: string;
  readonly visual: string;
  readonly states: string;
  readonly navigation: string;
  readonly traceability: string;
}

export interface Edition5Category {
  readonly code: string;
  readonly name: string;
  readonly controls: readonly string[];
}

export const E5_CONTROLS: readonly Edition5Control[] = [
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
    "catalogScreen": "S02",
    "defaultState": "Daemon catalog choice",
    "scope": "User/application; draft override",
    "applyTiming": "New conversation",
    "behavior": "Validate provider plus model together. If unavailable, keep the previous selection and offer Refresh; never silently use another endpoint.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [
      "faktor.defaultProvider",
      "faktor.run.defaultProvider"
    ],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 51,
    "currentStatus": "partial",
    "statusNote": "Default provider selection exists in both hosts; no served-instance CRUD/catalog validation surface."
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
      "S01",
      "S02"
    ],
    "catalogScreen": "S02",
    "defaultState": "Served catalog default",
    "scope": "User/application; conversation",
    "applyTiming": "New conversation or supported next task",
    "behavior": "Show full model ID, provider and current availability. A change requiring a new session offers Create new conversation with the draft preserved.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [
      "faktor.defaultModel",
      "faktor.run.defaultModel"
    ],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 51,
    "currentStatus": "partial",
    "statusNote": "Default model selection exists; no full capability-driven model picker."
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
    "catalogScreen": "S02",
    "defaultState": "None; local MRU",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Favorites refer to provider plus exact model ID. Removed models remain labelled unavailable until deliberately removed.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 51,
    "currentStatus": "missing",
    "statusNote": "No favorites/recent model list in either host."
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
    "catalogScreen": "S02",
    "defaultState": "Show effective daemon policy",
    "scope": "Daemon; captured task",
    "applyTiming": "Next admitted task",
    "behavior": "Pinned must never fall back silently; Economy explains selected and substituted model. Expose the actual route separately from the requested preference.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [
      "routing_mode"
    ],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 51,
    "currentStatus": "partial",
    "statusNote": "routing_mode/Pinned/Economy exists in config/router; no composer routing-policy control."
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
    "catalogScreen": "S02",
    "defaultState": "Inherit selected model",
    "scope": "Agent preset; task",
    "applyTiming": "Next child or model call after ack",
    "behavior": "Resolve exact model capabilities and shared budget for each role. A role model is not permission to create a child agent.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 52,
    "currentStatus": "partial",
    "statusNote": "Child model can be set; no per-role planner/coder/verifier/summarizer map."
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
    "catalogScreen": "S02",
    "defaultState": "No added fallback",
    "scope": "Task/daemon policy",
    "applyTiming": "Next routing decision",
    "behavior": "Explain trigger, price/data-boundary change and quality floor. Do not route local-only work to a hosted endpoint. Exhaustion is a visible failure.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 52,
    "currentStatus": "partial",
    "statusNote": "Router candidates/pricing/quality exist; no fallback-chain editor."
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
    "catalogScreen": "S02",
    "defaultState": "Served capability authority",
    "scope": "Provider/model",
    "applyTiming": "On catalog refresh",
    "behavior": "Show context, output, tools, parallel tools, reasoning, thinking, vision, documents, streaming and structured output. Unknown, generic declaration and exact-model authority remain distinct.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 52,
    "currentStatus": "partial",
    "statusNote": "GET /models serves capabilities; only a flat list is rendered, no capability sheet."
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
    "catalogScreen": "S02",
    "defaultState": "No automatic destructive probe",
    "scope": "Daemon instance",
    "applyTiming": "On explicit action",
    "behavior": "Show last observation time and probe failure. A context allocation probe does not certify tool-call or coding reliability.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 52,
    "currentStatus": "partial",
    "statusNote": "Provider registry/health is read; no explicit catalog refresh/probe action."
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
    "catalogScreen": "S02",
    "defaultState": "Served effective limit",
    "scope": "Model declaration; operator",
    "applyTiming": "After config validation and ack",
    "behavior": "Lower limits may constrain a model; larger declarations need supported authority. Show configured versus runtime-observed limits and reject impossible reservations.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [
      "model.contextLimit (proposed)"
    ],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 53,
    "currentStatus": "missing",
    "statusNote": "No context-window override editor; served limit is read-only."
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
    "catalogScreen": "S02",
    "defaultState": "Effective daemon floors",
    "scope": "Operator/task policy",
    "applyTiming": "Next route",
    "behavior": "Show user-declared versus observed quality and sample counts. Never relabel a manually entered score as a measured benchmark.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [
      "provider.quality (per-instance)"
    ],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 53,
    "currentStatus": "partial",
    "statusNote": "Quality floors/declarations exist in router/provider config; no UI."
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
      "S02",
      "S22",
      "S23"
    ],
    "catalogScreen": "S02",
    "defaultState": "No call yet",
    "scope": "Turn/call",
    "applyTiming": "On durable observation",
    "behavior": "Display provider, exact model, route reason and role on expanded calls. A composer preference must not masquerade as the model that executed.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 53,
    "currentStatus": "partial",
    "statusNote": "Turns carry actual provider/model; per-call route reason/detail is not surfaced in a dedicated identity sheet."
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
      "S02"
    ],
    "catalogScreen": "S02",
    "defaultState": "Current call unchanged",
    "scope": "Draft/session",
    "applyTiming": "After explicit acknowledgement",
    "behavior": "Changing a picker never rewrites an in-flight request. Unsupported mid-session changes offer a new session; show when the new value actually applies.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 53,
    "currentStatus": "partial",
    "statusNote": "Model is captured per task/child; no general mid-session switch-timing control."
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
      "S02",
      "S27"
    ],
    "catalogScreen": "S02",
    "defaultState": "Unavailable if not served",
    "scope": "Model/provider",
    "applyTiming": "At quote or observation",
    "behavior": "Separate list/declared/observed price and quota. Cache prices may be absent; missing rates do not become zero. No hard-coded model price recommendation.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 54,
    "currentStatus": "partial",
    "statusNote": "Usage/cost and provider pricing exist; no unified cost/limit sheet."
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
      "S02"
    ],
    "catalogScreen": "S02",
    "defaultState": "Off",
    "scope": "Task",
    "applyTiming": "New comparison admission",
    "behavior": "Name all candidate models, shared cap and evidence basis before running. Compare verified outcomes and attempted spend; keep candidate identity and user choice explicit.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Model-adapter parameter descriptors + normalized effective call receipt (p.010, p.028)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "GET /capabilities",
      "GET /native/providers"
    ],
    "featureRefs": [
      "F01",
      "F06",
      "F07",
      "F09",
      "B02",
      "B12"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A03",
      "A05",
      "A31",
      "A36"
    ],
    "specPage": 54,
    "currentStatus": "partial",
    "statusNote": "Tournament compares candidate runs; no model-comparison workflow."
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
      "S03"
    ],
    "catalogScreen": "S03",
    "defaultState": "User supplied label",
    "scope": "Machine/daemon",
    "applyTiming": "After save",
    "behavior": "Show readable label and reveal exact ID. Validate uniqueness before storing. Renaming a label must not move credentials or alias a different endpoint.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].id"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 55,
    "currentStatus": "partial",
    "statusNote": "Stable provider id exists in config; no connection label/ID editor."
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
    "catalogScreen": "S03",
    "defaultState": "Detected suggestion, user confirmed",
    "scope": "Connection",
    "applyTiming": "After save/restart if required",
    "behavior": "Distinguish OpenAI Chat, Responses and other actually supported adapters. Never infer a remote API family from a marketing model name.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].kind"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 55,
    "currentStatus": "partial",
    "statusNote": "API family is config-only; picker is read-only."
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
    "catalogScreen": "S03",
    "defaultState": "Adapter default, shown explicitly",
    "scope": "Machine/connection",
    "applyTiming": "After validation and connection ack",
    "behavior": "Show which machine resolves the URL. Reject invalid schemes, userinfo and blocked destinations; keep refusal inline without exposing credential fragments.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].base_url"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 55,
    "currentStatus": "partial",
    "statusNote": "base_url config-only; no validated endpoint editor."
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
      "S03"
    ],
    "catalogScreen": "S03",
    "defaultState": "No credential value",
    "scope": "Machine secret store",
    "applyTiming": "On next authorized request",
    "behavior": "Current provider config uses environment references. Direct IDE provider-key entry requires a secure daemon handoff contract; never put key values in shared settings or exports.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].api_key_env"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 55,
    "currentStatus": "partial",
    "statusNote": "api_key_env references exist; no secure daemon vault handoff for provider keys."
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
    "catalogScreen": "S03",
    "defaultState": "None",
    "scope": "Connection, secret values in vault",
    "applyTiming": "After save",
    "behavior": "Current adapter helper bounds: 32 headers, names 128 bytes, values 8192 bytes. Native editor persistence and vault binding still need implementation. Mask auth-shaped values.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].headers (adapter ExtraHeaders)"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 56,
    "currentStatus": "partial",
    "statusNote": "ExtraHeaders is validated in the provider layer; no native persistence/editor."
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
    "catalogScreen": "S03",
    "defaultState": "Served or explicitly declared IDs",
    "scope": "Connection",
    "applyTiming": "After catalog reload",
    "behavior": "Display live, exact declaration or generic declaration provenance. Failed discovery does not erase explicit IDs or falsely validate tool calling.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].models"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 56,
    "currentStatus": "partial",
    "statusNote": "Explicit custom model ids are config-only; no discovery/declaration editor."
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
    "catalogScreen": "S03",
    "defaultState": "Existing configured local adapter",
    "scope": "Connection",
    "applyTiming": "After readiness",
    "behavior": "Expose the protocol being used, not a promise of support for every inference server. LM Studio/vLLM are endpoint examples only when that compatible path is configured.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].kind=ollama"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 56,
    "currentStatus": "partial",
    "statusNote": "Ollama/local engine selection is config-only."
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
    "catalogScreen": "S03",
    "defaultState": "Adapter default; show actual value",
    "scope": "Machine/connection",
    "applyTiming": "After validated config reload",
    "behavior": "Ollama defaults to permitted loopback; custom OpenAI uses explicit opt-in. This switch does not authorize private LAN, link-local or metadata destinations.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "providers[].allow_loopback"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 56,
    "currentStatus": "partial",
    "statusNote": "allow_loopback is typed per adapter; no UI."
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
    "catalogScreen": "S03",
    "defaultState": "Not available under current provider rule",
    "scope": "Machine/operator policy",
    "applyTiming": "Only after new backend policy exists",
    "behavior": "A factory-LAN address cannot be enabled by allow_loopback. Show the current typed refusal; scoped authenticated LAN policy is new work, not a hidden workaround toggle.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "sandbox.network (proposed LAN policy)"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 57,
    "currentStatus": "missing",
    "statusNote": "Private-LAN inference is refused by current policy; no scoped backend implementation."
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
    "catalogScreen": "S03",
    "defaultState": "No request until selected",
    "scope": "Connection",
    "applyTiming": "On explicit action",
    "behavior": "Show stages and target; note if the chosen probe incurs provider usage. Auth OK, model listed and model capable are separate results.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 57,
    "currentStatus": "missing",
    "statusNote": "No connection test/probe route or UI."
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
    "catalogScreen": "S03",
    "defaultState": "Effective adapter policy",
    "scope": "Connection",
    "applyTiming": "Next request after ack",
    "behavior": "Show connect, first-byte and total deadlines where supported. Do not retry a request whose external effect or streamed admission is unknown.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "provider transport timeout (adapter)"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 57,
    "currentStatus": "partial",
    "statusNote": "Transport timeouts exist in adapter code, not as an editable native setting."
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
    "catalogScreen": "S03",
    "defaultState": "System/daemon policy",
    "scope": "Machine/operator",
    "applyTiming": "After reload",
    "behavior": "Show effective proxy and certificate source without secrets. Do not provide a convenient disable-TLS-verification checkbox as an ordinary setting.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 57,
    "currentStatus": "missing",
    "statusNote": "No proxy/TLS/certificate configuration surface."
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
    "catalogScreen": "S03",
    "defaultState": "Current daemon host",
    "scope": "Connection/session",
    "applyTiming": "On binding",
    "behavior": "Local means local to the daemon, not necessarily the laptop. SSH/container sessions show endpoint reachability, vault location and file-resolution host.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 58,
    "currentStatus": "partial",
    "statusNote": "Execution host is visible in bindings/diagnostics; no remote execution-location editor."
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
    "catalogScreen": "S03",
    "defaultState": "Keep active binding until replaced",
    "scope": "Machine/connection",
    "applyTiming": "After acknowledgement",
    "behavior": "Rotation is write-only. Delete shows affected drafts and sessions; active requests retain their captured credential generation, with clear failure if revoked.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 58,
    "currentStatus": "partial",
    "statusNote": "Control-plane sign-out/credential removal exists; provider reconnect/rotate/delete does not."
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
    "catalogScreen": "S03",
    "defaultState": "Inherit runtime policy",
    "scope": "Local connection/operator",
    "applyTiming": "After explicit runtime acknowledgement",
    "behavior": "Show configured versus observed allocation. Offer only supported engine controls; unloading needs ownership and active-use checks. No generic GPU or thread sliders without a real engine contract.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02",
      "BK10"
    ],
    "configKeys": [
      "ollama.keep_alive"
    ],
    "runtimeRequired": [
      "Provider lifecycle create/update/test/delete with stable connection id, API family, endpoint, secret reference; separate reachability/auth/model-list/capability probes (p.028)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "GET /models"
    ],
    "featureRefs": [
      "F08",
      "F37",
      "F21",
      "B03"
    ],
    "acceptanceIds": [
      "A01",
      "A02",
      "A06",
      "A07",
      "A08",
      "A10",
      "A23"
    ],
    "specPage": 58,
    "currentStatus": "partial",
    "statusNote": "Ollama keep_alive/allocation is adapter config; no owned runtime controls."
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
    "catalogScreen": "S05",
    "defaultState": "Inherit model/daemon",
    "scope": "Model default; draft/task override",
    "applyTiming": "Next admitted call",
    "behavior": "Use the adapter-offered values. Current core enum has four levels; do not expose Max or a made-up numeric range for every model.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [
      "model.reasoning (core enum)"
    ],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "partial",
    "statusNote": "Reasoning mode exists in the model/task envelope; not a writable per-model tuning API."
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
    "catalogScreen": "S05",
    "defaultState": "Provider/model default",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Keep thinking budget separate from total output reserve. Hide if the adapter cannot control it; explain any shared reasoning/output accounting.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Served maxOutput",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Validate against context and output capabilities. Increasing output reserve reduces space for input; warn before discarding or compacting selected context.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [
      "task.max_tokens"
    ],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "partial",
    "statusNote": "max_tokens is captured per task; not an effective model tuning form."
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
    "catalogScreen": "S05",
    "defaultState": "Provider default; unset stays unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Unsupported models show Not supported rather than a nonfunctional slider. Default reset removes the override; never send a null or arbitrary zero accidentally.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Validate the exact adapter range. Warn on mutually restrictive sampling choices; preserve provider defaults unless the user sets a value.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Only show when the selected adapter accepts it. A local runtime option is not automatically a hosted-model capability.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 59,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Render independent labels, ranges and reset actions. Unsupported request fields must be rejected before submission, not silently ignored.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Unset",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "A seed is best-effort provider behavior, not reproducibility proof. Show whether the provider acknowledged it and retain it in the captured request summary.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "None",
    "scope": "Model/task",
    "applyTiming": "Next call",
    "behavior": "Warn when stops can truncate structured tool output. Do not allow an advanced text setting to corrupt native tool-call framing.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Effective supported policy",
    "scope": "Agent/task",
    "applyTiming": "Next model call",
    "behavior": "Needs both model support and runtime-safe scheduling. Concurrency must not bypass shared budget, capability grants, cancellation or edit ownership.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Normal agent protocol",
    "scope": "Task",
    "applyTiming": "Next supported operation",
    "behavior": "Validate schema size and compatibility. Do not replace Faktor internal tool envelopes with user-supplied formatting or claim JSON merely because prose looks structured.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Provider default",
    "scope": "Connection/task policy",
    "applyTiming": "Next call",
    "behavior": "Display actual tier and cache-write/read basis. A cache toggle must not claim that a prefix hit occurred; observe that separately.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 60,
    "currentStatus": "missing",
    "statusNote": "No adapter parameter descriptor/editor; current request DTOs are strict and do not expose these fields."
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
    "catalogScreen": "S05",
    "defaultState": "Stream when supported",
    "scope": "User/model",
    "applyTiming": "Next call",
    "behavior": "Preserve stop, stale-state and transcript semantics when disabled. A quiet model call is not an idle task; show request phase without fake progress.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [
      "streaming (adapter)"
    ],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 61,
    "currentStatus": "partial",
    "statusNote": "Streaming is implemented and rendered; no supported on/off preference or request-phase detail."
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
      "S05",
      "S22"
    ],
    "catalogScreen": "S05",
    "defaultState": "No request captured yet",
    "scope": "Draft and immutable call receipt",
    "applyTiming": "Before admission / after ack",
    "behavior": "List requested, effective and unsupported fields with source and timing. Do not dump hidden system secrets or claim to expose private model reasoning.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK02"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Capability-driven parameter descriptors and normalized effective request receipt; unsupported fields rejected before dispatch (p.028, p.039)."
    ],
    "runtimeEndpoints": [
      "GET /models",
      "POST /native/session/{id}/task-runs"
    ],
    "featureRefs": [
      "F09",
      "F07",
      "B03",
      "B12"
    ],
    "acceptanceIds": [
      "A02",
      "A03",
      "A04",
      "A05"
    ],
    "specPage": 61,
    "currentStatus": "missing",
    "statusNote": "No normalized effective-request preview/receipt UI."
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
      "S01",
      "S06"
    ],
    "catalogScreen": "S06",
    "defaultState": "Code as proposed initial preset",
    "scope": "User default; draft",
    "applyTiming": "Before admission",
    "behavior": "Describe scope and tool envelope in the picker. Existing Task/Ask controls are not equivalent to enforced read-only Ask; the stricter preset needs backend enforcement.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Task/Ask entry and agent/model controls exist; no executable preset picker."
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
    "catalogScreen": "S06",
    "defaultState": "No writes or effectful tools",
    "scope": "Preset/task",
    "applyTiming": "At task admission",
    "behavior": "Allow exploration and answers; filter tools and deny writes at dispatch. Current plain prompt/Ask route alone does not prove read-only operation.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Ask prompt path exists; it is not dispatch-enforced read-only, so the label must stay honest."
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
    "catalogScreen": "S06",
    "defaultState": "No code edits",
    "scope": "Preset/task",
    "applyTiming": "At admission",
    "behavior": "Show plan file and review comments. User approval admits a new Code task with a frozen plan version; it does not mutate the planning run in place.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Plan subview and task contracts exist; no frozen-plan-to-Code admission binding."
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
    "catalogScreen": "S06",
    "defaultState": "Inherit baseline instructions",
    "scope": "User/project preset",
    "applyTiming": "Next task",
    "behavior": "Preview the merged instruction bundle and token estimate. Prompt text cannot widen a capability grant or override managed policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Instructions exist in config; no custom role-instruction preset editor."
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
    "catalogScreen": "S06",
    "defaultState": "Effective preset envelope",
    "scope": "Preset/task",
    "applyTiming": "At admission",
    "behavior": "Show inherited denies and exact MCP namespaces. A newly discovered tool remains unavailable until policy resolution allows it.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Per-tool decisions exist at dispatch; no preset tool allowlist editor."
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
      "S06",
      "S07",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "Inherit",
    "scope": "Preset/task/child",
    "applyTiming": "Next admitted child/call",
    "behavior": "Show fallback and pricing boundary. Existing child model control is preserved; richer preset defaults need explicit configuration and adapter mapping.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 62,
    "currentStatus": "partial",
    "statusNote": "Child model/steer controls exist; no role model/reasoning preset."
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
      "S06",
      "S07",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "Effective runtime policy",
    "scope": "Task",
    "applyTiming": "Before new child admission",
    "behavior": "Show max depth, parent budget share and allowed child roles. Disabling delegation prevents new children; it does not pretend to stop already-running children.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "partial",
    "statusNote": "Child delegation exists; no delegation-policy editor."
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
      "S06",
      "S07",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "Effective daemon limit",
    "scope": "Task/operator",
    "applyTiming": "Next child admission",
    "behavior": "Display waiting versus active children. Saturation creates an owned bounded queue or a typed refusal; never unbounded spawning.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "partial",
    "statusNote": "Concurrency is bounded by the orchestrator; no user-facing limit control."
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
      "S06",
      "S07",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "Shared parent envelope",
    "scope": "Task/child",
    "applyTiming": "On grant acknowledgement",
    "behavior": "A child never gets an independent unlimited wallet. Preserve open reservations and uncertain spend when reducing an allowance.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "partial",
    "statusNote": "Shared parent budget ledger exists; no child allocation editor."
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
      "S06",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "No action until selected",
    "scope": "Child run",
    "applyTiming": "After durable control ack",
    "behavior": "Existing steer/model/agent controls are reused where served. Steer text retains the current 500-character bound; unsupported pause is disabled, not simulated.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "exists",
    "statusNote": "Pause/resume/cancel/retry/steer/model child controls are implemented."
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
      "S06",
      "S07",
      "S23",
      "S24",
      "S31"
    ],
    "catalogScreen": "S06",
    "defaultState": "Isolated candidate",
    "scope": "Task/child",
    "applyTiming": "Before edit ownership grant",
    "behavior": "Keep parallel agents from overwriting the same candidate. Shared-file contention shows owner, conflict and handoff; agents do not acquire arbitrary owner-checkout write access.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "partial",
    "statusNote": "Shadow candidate ownership is enforced; no worktree/edit-ownership control."
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
      "S06",
      "S07",
      "S23"
    ],
    "catalogScreen": "S06",
    "defaultState": "Existing task evidence where served",
    "scope": "Child to parent",
    "applyTiming": "On child completion",
    "behavior": "Show what was requested, returned, verified and still missing. Child text saying done is not the parent task completion basis.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK05",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Preset/rule CRUD with version/source/scope; effective envelope at admission (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /native/agents",
      "POST /native/agents/{child}/pause|resume|cancel|retry|steer|model"
    ],
    "featureRefs": [
      "F04",
      "F28",
      "F38",
      "F33",
      "B05",
      "B16"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A25",
      "A26",
      "A38"
    ],
    "specPage": 63,
    "currentStatus": "partial",
    "statusNote": "Typed completion/handoff artifacts exist; no schema editor."
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
      "S08"
    ],
    "catalogScreen": "S08",
    "defaultState": "Unavailable until authoritative read",
    "scope": "Call/session",
    "applyTiming": "On measured snapshot",
    "behavior": "Current projection contextUsage is null. Add a durable versioned read contract before showing real percentages; keep draft estimates explicitly labelled.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A11",
      "A12"
    ],
    "specPage": 64,
    "currentStatus": "missing",
    "statusNote": "Projection contextUsage is null; no durable context meter."
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
      "S08"
    ],
    "catalogScreen": "S08",
    "defaultState": "Measured or estimated categories",
    "scope": "Draft/call",
    "applyTiming": "On preview / after call",
    "behavior": "Categories must be mutually accountable and sum to the same basis. Output reserve and safety margin are not consumed input tokens.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A11",
      "A12"
    ],
    "specPage": 64,
    "currentStatus": "missing",
    "statusNote": "No context breakdown/accounting API or UI."
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
      "S09",
      "S20"
    ],
    "catalogScreen": "S08",
    "defaultState": "Nothing silently attached",
    "scope": "Draft",
    "applyTiming": "On explicit selection",
    "behavior": "Show host URI, range, size and include/exclude state. A reference from another worktree resolves there, never against a global current directory.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A13"
    ],
    "specPage": 64,
    "currentStatus": "partial",
    "statusNote": "Attachments/mentions exist; no unified URI/range/version picker."
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
    "catalogScreen": "S08",
    "defaultState": "Proposed: selection only with visible chip",
    "scope": "User/workspace default",
    "applyTiming": "At draft capture",
    "behavior": "Display every automatic inclusion. A tab title is not its contents; enabling all open files must show the added input and file scope.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A13"
    ],
    "specPage": 64,
    "currentStatus": "partial",
    "statusNote": "Editor context is captured via attachments/files; no automatic-inclusion chip model."
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
      "S08",
      "S09"
    ],
    "catalogScreen": "S08",
    "defaultState": "Existing typed upload behavior",
    "scope": "Session/draft",
    "applyTiming": "On upload and admission",
    "behavior": "Preserve current MIME/byte gates from the served exact model. Unsupported image/document attachments remain visible with a reason, never silently dropped.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A13",
      "A14"
    ],
    "specPage": 65,
    "currentStatus": "exists",
    "statusNote": "Typed attachment staging and model-aware validation are implemented."
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
      "S09"
    ],
    "catalogScreen": "S08",
    "defaultState": "Respect model capability",
    "scope": "Draft",
    "applyTiming": "On paste / send",
    "behavior": "Collapse large text behind a preview while preserving full bounded content. Show an invalid image or oversize payload without clearing the rest of the draft.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A14"
    ],
    "specPage": 65,
    "currentStatus": "partial",
    "statusNote": "Large paste is handled in the host webview; no preview/bounded attachment policy UI."
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
    "catalogScreen": "S08",
    "defaultState": "No implicit pinned content",
    "scope": "Draft/task",
    "applyTiming": "Next captured request",
    "behavior": "Pinning keeps priority, not permission to exceed a hard window. Explain when an item cannot fit and require a deliberate trim or alternate model.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 65,
    "currentStatus": "missing",
    "statusNote": "No pin/exclude context-item control."
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
    "catalogScreen": "S08",
    "defaultState": "Existing tool/security rules remain authoritative",
    "scope": "Project/user policy",
    "applyTiming": "After policy ack",
    "behavior": "Separate search exclusions from security denies. A retrieval ignore file is not a guarantee that an authorized shell cannot read a file.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 65,
    "currentStatus": "partial",
    "statusNote": "Index/retrieval exclusions exist in config; no ignore/protected pattern editor."
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
    "catalogScreen": "S08",
    "defaultState": "Effective available strategy",
    "scope": "Daemon/task",
    "applyTiming": "Next retrieval",
    "behavior": "No configured embedder means lexical/symbol fallback. Never label the index semantic merely because the UI contains that option.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "partial",
    "statusNote": "Retrieval strategy/embedding config and semantic status exist; no strategy picker."
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
    "catalogScreen": "S08",
    "defaultState": "No embedder when absent",
    "scope": "Daemon/operator",
    "applyTiming": "After validated reload/reindex",
    "behavior": "Model changes invalidate incompatible vectors and show rebuild state. Credentials and data egress are disclosed before embedding a codebase.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "partial",
    "statusNote": "Embedding provider/model config exists; no boundary-reviewed setup UI."
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
    "catalogScreen": "S08",
    "defaultState": "Current index snapshot",
    "scope": "Workspace/index",
    "applyTiming": "On status refresh",
    "behavior": "Show indexed, ignored, stale and failed paths and commit basis. Existing status/coverage endpoints are reused; do not infer completion from a background spinner.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "partial",
    "statusNote": "Index coverage/freshness is served and rendered; no coverage details screen."
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
    "catalogScreen": "S08",
    "defaultState": "Block an over-cap request",
    "scope": "Draft/call",
    "applyTiming": "Before admission",
    "behavior": "Offer remove, shorten, compact or choose a capable model. No hidden truncation of acceptance criteria, permissions or captured engineering obligations.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "partial",
    "statusNote": "Context budget math exists; no admission guard UI."
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
    "catalogScreen": "S08",
    "defaultState": "Read-only",
    "scope": "Context item",
    "applyTiming": "On selection",
    "behavior": "Show exact version/range and how it entered context. Treat tool output, fetched pages and repository instructions as data, not authority to change policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "missing",
    "statusNote": "No source preview/provenance panel."
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
    "catalogScreen": "S08",
    "defaultState": "Exclude credentials and secret payloads",
    "scope": "User/session",
    "applyTiming": "On explicit export",
    "behavior": "Include IDs, hashes, basis and omission reasons. Mark estimates; do not export a guessed hidden prompt or private model chain of thought.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned context manifest/accounting with exact basis, observed input, reserves and unavailable reasons (p.029, p.040)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "POST /native/session/{id}/attachments"
    ],
    "featureRefs": [
      "F07",
      "F23",
      "F27",
      "F18",
      "B06"
    ],
    "acceptanceIds": [
      "A08",
      "A11",
      "A12",
      "A13",
      "A14",
      "A15",
      "A38"
    ],
    "specPage": 66,
    "currentStatus": "missing",
    "statusNote": "No context export."
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
    "catalogScreen": "S10",
    "defaultState": "Daemon default 0.65",
    "scope": "Daemon; future task override",
    "applyTiming": "Next compaction decision",
    "behavior": "Use the daemon usage denominator. Show effective threshold and read source; do not reuse a graphical estimated context ratio as authoritative runtime usage.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "compact_at_usage"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 67,
    "currentStatus": "partial",
    "statusNote": "compact_at_usage/compaction_model exist in config; no compaction control surface."
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
    "catalogScreen": "S10",
    "defaultState": "Inherit when absent",
    "scope": "Daemon/operator",
    "applyTiming": "Next compaction",
    "behavior": "Show egress and cost implications and required capabilities. An unavailable compactor preserves the last valid state and reports failure.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "compaction_model"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 67,
    "currentStatus": "partial",
    "statusNote": "compact_at_usage/compaction_model exist in config; no compaction control surface."
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
    "catalogScreen": "S10",
    "defaultState": "No automatic manual action",
    "scope": "Session",
    "applyTiming": "After durable operation ack",
    "behavior": "Requires an explicit new native operation contract. Show preserved facts, removed material and reduction; a failed minimum reduction cannot be labelled successful.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A15"
    ],
    "specPage": 67,
    "currentStatus": "missing",
    "statusNote": "No manual compaction operation or post-compaction inspection API/UI."
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
    "catalogScreen": "S10",
    "defaultState": "Latest successful compaction",
    "scope": "Session",
    "applyTiming": "After completion",
    "behavior": "Retain immutable original history by reference, summary version and stale evidence flags. Re-expanding UI text does not restore model context automatically.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A15"
    ],
    "specPage": 67,
    "currentStatus": "missing",
    "statusNote": "No manual compaction operation or post-compaction inspection API/UI."
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
    "catalogScreen": "S10",
    "defaultState": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose the real efficiency.failure_learning off-switch. Separate learned failure priors from personal memories and show disabled/fallback status.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "efficiency.failure_learning"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 68,
    "currentStatus": "partial",
    "statusNote": "Production efficiency flags exist and are ON by default; no settings UI/effect display."
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
    "catalogScreen": "S10",
    "defaultState": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.ccr. Disabling it does not remove bounded-output limits; show the durable full artifact and bounded model excerpt separately.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "efficiency.ccr"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 68,
    "currentStatus": "partial",
    "statusNote": "Production efficiency flags exist and are ON by default; no settings UI/effect display."
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
    "catalogScreen": "S10",
    "defaultState": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.typed_handoff. Changing it never erases the task ledger; describe the current projection basis and loss of structured handoff benefits.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "efficiency.typed_handoff"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 68,
    "currentStatus": "partial",
    "statusNote": "Production efficiency flags exist and are ON by default; no settings UI/effect display."
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
    "catalogScreen": "S10",
    "defaultState": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Canonical key semantic_context; information_gain is an alias, not another switch. Duplicate aliases must fail validation instead of silently winning.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "efficiency.semantic_context (alias information_gain)"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 68,
    "currentStatus": "partial",
    "statusNote": "Production efficiency flags exist and are ON by default; no settings UI/effect display."
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
    "catalogScreen": "S10",
    "defaultState": "Production ON",
    "scope": "Daemon/operator",
    "applyTiming": "After config reload",
    "behavior": "Expose efficiency.rework_routing and distinguish prior declarations from verified outcome observations. No measured savings claim without a matching denominator.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [
      "efficiency.rework_routing"
    ],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 68,
    "currentStatus": "partial",
    "statusNote": "Production efficiency flags exist and are ON by default; no settings UI/effect display."
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
      "S12"
    ],
    "catalogScreen": "S10",
    "defaultState": "Proposed: manual only",
    "scope": "User/project; separate stores",
    "applyTiming": "Next request after ack",
    "behavior": "New memory authority needs list/read/edit/delete, source, timestamp and retention. Do not reinterpret current failure-learning records as general auto-memory.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A16"
    ],
    "specPage": 68,
    "currentStatus": "missing",
    "statusNote": "Typed memory facts exist; no native list/edit/delete retention UI."
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
      "S12"
    ],
    "catalogScreen": "S10",
    "defaultState": "No cross-project sharing",
    "scope": "Memory record",
    "applyTiming": "After durable save/delete ack",
    "behavior": "Deleting a memory does not remove earlier prompt copies. Show downstream retention scope; prevent a project from selecting an arbitrary global memory location.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A16"
    ],
    "specPage": 69,
    "currentStatus": "missing",
    "statusNote": "Typed memory facts exist; no native list/edit/delete retention UI."
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
      "S27",
      "S28"
    ],
    "catalogScreen": "S10",
    "defaultState": "Unavailable when not recorded",
    "scope": "Session/workspace",
    "applyTiming": "After measurement",
    "behavior": "Show compression reduction, cache observations and routing outcomes separately. No fabricated percent saved or zero-cost assumption for locally hosted inference.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK04",
      "BK07"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned compaction operation with cancellation, retained-facts manifest and measured result; memory CRUD with provenance/retention (p.029, p.043)."
    ],
    "runtimeEndpoints": [
      "GET /session/{id}/projection",
      "GET /native/semantic/status"
    ],
    "featureRefs": [
      "F10",
      "F11",
      "F27",
      "B18"
    ],
    "acceptanceIds": [
      "A12",
      "A15",
      "A16",
      "A31",
      "A36"
    ],
    "specPage": 69,
    "currentStatus": "missing",
    "statusNote": "No efficiency effect display."
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
    "catalogScreen": "S11",
    "defaultState": "Effective daemon baseline",
    "scope": "User/daemon",
    "applyTiming": "Next request after ack",
    "behavior": "Keep unsaved text local. Show token estimate and source; do not claim a new native instructions API is already present.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [
      "instructions"
    ],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 70,
    "currentStatus": "partial",
    "statusNote": "Daemon instructions exist; no attributed user instruction editor."
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
    "catalogScreen": "S11",
    "defaultState": "Discover only supported trusted sources",
    "scope": "Project",
    "applyTiming": "Next request after reload/ack",
    "behavior": "Display path, trust and active workspace. The repository guide is not proof that the runtime discovers every nested AGENTS.md file today.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [
      "AGENTS.md"
    ],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 70,
    "currentStatus": "partial",
    "statusNote": "AGENTS.md/discovery exists in instructions; no project instruction management UI."
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
    "catalogScreen": "S11",
    "defaultState": "No new rule enabled silently",
    "scope": "Project/user",
    "applyTiming": "Next matching task",
    "behavior": "Validate glob and test against sample paths. Explain always/manual/path-matched activation and show the matching source in the request preview.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [
      ".faktor/rules (proposed)"
    ],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 70,
    "currentStatus": "partial",
    "statusNote": "Scoped instruction discovery exists; no rule activation editor."
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
      "S11"
    ],
    "catalogScreen": "S11",
    "defaultState": "Backend-served resolution",
    "scope": "Task/session",
    "applyTiming": "On config snapshot",
    "behavior": "Separate enforced managed constraints from instruction text. Proposed ordinary preference order is task, project, user, daemon default; fields may restrict allowed scopes.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 70,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "Existing effective set",
    "scope": "Rule/source",
    "applyTiming": "After acknowledged reload",
    "behavior": "Show inactive, invalid, disabled and locked states. Disabling one file must not disable an unrelated rule with a matching display name.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "None",
    "scope": "User/project",
    "applyTiming": "On insert into draft",
    "behavior": "Template insertion does not execute tools or send a prompt. Required variables remain visibly unresolved until filled; preserve the original draft on cancel.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [
      "profiles/commands (proposed)"
    ],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "No silent conflict resolution in UI",
    "scope": "Effective config",
    "applyTiming": "On validation",
    "behavior": "Show winner and reason, with links to editable sources. A locked policy cannot be overridden by appending a more forceful instruction.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
      "S11"
    ],
    "catalogScreen": "S11",
    "defaultState": "No import executed automatically",
    "scope": "User/project",
    "applyTiming": "After explicit import save",
    "behavior": "Import supported semantics only. foreign-editor/Claude config files are not native Faktor formats; unsupported keys remain a visible migration report.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "No secret/system dump",
    "scope": "Task/call",
    "applyTiming": "Before admission when available",
    "behavior": "Show user-editable instructions and loaded-source manifest. Do not claim to reveal hidden provider instructions or protected model internals.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "Project/user preference",
    "scope": "User/preset",
    "applyTiming": "Next request",
    "behavior": "A communication style affects prose, not tool grants, verification policy or data egress. Show it in the effective prompt preview without occupying the main toolbar.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 71,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
    "catalogScreen": "S11",
    "defaultState": "Captured at admission",
    "scope": "Task/call",
    "applyTiming": "At durable admission",
    "behavior": "Running work retains its captured bundle. Editing a file shows Next task or Restart required; it must not silently rewrite the instructions of an existing run.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Versioned instruction/rule content with origin, activation scope and effective ordering (p.010, p.043)."
    ],
    "runtimeEndpoints": [
      "(no native instruction route)"
    ],
    "featureRefs": [
      "F11",
      "F33",
      "B11",
      "B27"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A15",
      "A23"
    ],
    "specPage": 72,
    "currentStatus": "missing",
    "statusNote": "No instructions/rules effective-order, enablement, templates, conflict, import, system-prompt, style or version-binding UI."
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
      "S01",
      "S13"
    ],
    "catalogScreen": "S13",
    "defaultState": "Proposed: inspect/read and ask before effects",
    "scope": "Task/user within managed bounds",
    "applyTiming": "At admission; revocation acknowledged",
    "behavior": "Explain the exact tool categories affected. Avoid an ambiguous full-auto switch; full privilege is an explicit advanced policy, never inferred from Code mode.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "permission preset (proposed)"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 73,
    "currentStatus": "partial",
    "statusNote": "Permission decisions exist at admission; no preset envelope editor."
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
    "catalogScreen": "S13",
    "defaultState": "Ask when no authorized rule applies",
    "scope": "User/project/managed",
    "applyTiming": "After policy reload/ack",
    "behavior": "Persistent rules require a real policy store and dispatcher enforcement. Current permission-reply API alone cannot save an Always allow rule.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "permission-rules store (proposed)"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 73,
    "currentStatus": "partial",
    "statusNote": "One-shot reply exists; persistent per-tool rules require a new policy store."
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
    "catalogScreen": "S13",
    "defaultState": "Captured workspace/candidate",
    "scope": "Rule/task",
    "applyTiming": "At each dispatch",
    "behavior": "Distinguish read, create, edit and delete. Normalize symlinks and worktree ownership server-side; a pretty glob is not a containment guarantee.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "sandbox.filesystem (proposed)"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 73,
    "currentStatus": "partial",
    "statusNote": "Path/command/network checks exist in trusted adapters; no matcher editor."
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
    "catalogScreen": "S13",
    "defaultState": "Ask for unclassified commands",
    "scope": "Rule/task",
    "applyTiming": "At each dispatch",
    "behavior": "Preview the exact normalized command. Shell injection/composition and interpreter scripts must not be approved through a misleading prefix match.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "sandbox.shell"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 73,
    "currentStatus": "partial",
    "statusNote": "Path/command/network checks exist in trusted adapters; no matcher editor."
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
    "catalogScreen": "S13",
    "defaultState": "Effective daemon deny/allow rules",
    "scope": "Operator/task",
    "applyTiming": "At dispatch",
    "behavior": "Read-only HTTP, authenticated API writes, uploads and messages have different effects. Show destination class and data boundary, not only a tool name.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "sandbox.network"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 74,
    "currentStatus": "partial",
    "statusNote": "Path/command/network checks exist in trusted adapters; no matcher editor."
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
    "catalogScreen": "S13",
    "defaultState": "Pending current request",
    "scope": "Session/request generation",
    "applyTiming": "After exact reply ack",
    "behavior": "Preserve existing target/reason and stale/wrong-session handling. Disable duplicate reply buttons until a durable or typed outcome arrives.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A17"
    ],
    "specPage": 74,
    "currentStatus": "exists",
    "statusNote": "One-shot approval card and exact generated reply are implemented."
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
      "S13",
      "S14"
    ],
    "catalogScreen": "S13",
    "defaultState": "Off",
    "scope": "Named policy scope",
    "applyTiming": "Future matching dispatch only",
    "behavior": "A remembered rule cannot pre-authorize another pending ID. Show match scope and expiry; unavailable persistence leaves Allow once as the only approval option.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A18"
    ],
    "specPage": 74,
    "currentStatus": "missing",
    "statusNote": "Remember-decision/persistent-rule draft is unavailable without a policy store."
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
    "catalogScreen": "S13",
    "defaultState": "Read-only simulator",
    "scope": "Rule draft",
    "applyTiming": "Before save",
    "behavior": "Show which normalized rule wins and why. Simulator output is advisory until the runtime returns the same versioned effective policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A18"
    ],
    "specPage": 74,
    "currentStatus": "missing",
    "statusNote": "No rule simulator/preview UI."
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
      "S13",
      "S14",
      "S25"
    ],
    "catalogScreen": "S13",
    "defaultState": "One request for one-shot approval",
    "scope": "Grant/rule",
    "applyTiming": "After revocation ack",
    "behavior": "Revoke prevents future effects and reports active operations separately. Do not imply an already-issued remote effect can be undone by revoking a local grant.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A17",
      "A28"
    ],
    "specPage": 75,
    "currentStatus": "missing",
    "statusNote": "No expiry/revocation rule model."
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
      "S13"
    ],
    "catalogScreen": "S13",
    "defaultState": "Actual served platform state",
    "scope": "Daemon host",
    "applyTiming": "On readiness/status refresh",
    "behavior": "Show filesystem, shell and network guarantees separately. Network-capable fallback is not a fully isolated shell. A failed setup stays a real blocker.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "sandbox.network_guarantee"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 75,
    "currentStatus": "partial",
    "statusNote": "Sandbox policy/status exists; no guarantees panel."
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
      "S13"
    ],
    "catalogScreen": "S13",
    "defaultState": "Current fail-closed default",
    "scope": "Machine/operator",
    "applyTiming": "After durable choice and restart if required",
    "behavior": "Reuse current faktor sandbox choose guidance until a real native setup API exists. Do not change shell security through an unacknowledged webview toggle.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "sandbox.shell"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 75,
    "currentStatus": "partial",
    "statusNote": "Sandbox setup is config/first-run; no native setup choice UI."
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
      "S13"
    ],
    "catalogScreen": "S13",
    "defaultState": "No managed assumptions when disabled",
    "scope": "Organization/machine",
    "applyTiming": "On effective-config read",
    "behavior": "A locked setting cannot be relaxed by user/project defaults. Local coding remains available when the optional control plane is disabled.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [
      "enterprise.settings"
    ],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 75,
    "currentStatus": "partial",
    "statusNote": "Managed enterprise configuration exists; no lock/provenance inspector."
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
      "S13",
      "S14"
    ],
    "catalogScreen": "S13",
    "defaultState": "Only recorded decisions",
    "scope": "Task/organization when enabled",
    "applyTiming": "After durable event",
    "behavior": "Show actor, scope, time, target and outcome; redact payload secrets. Permission requested, allowed, dispatched and effect confirmed remain different facts.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 76,
    "currentStatus": "missing",
    "statusNote": "No permission decision audit surface."
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
    "catalogScreen": "S13",
    "defaultState": "Untrusted retrieved content",
    "scope": "Tool/source item",
    "applyTiming": "On dispatch and review",
    "behavior": "A file, MCP result or webpage cannot instruct the UI to elevate authority. Show why an instruction was treated as data without blocking ordinary legitimate content.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Shared evaluator for preview/dispatch; one-shot waiter; persistent versioned policy store (p.030, p.041)."
    ],
    "runtimeEndpoints": [
      "GET /native/permissions",
      "POST /native/permission/reply"
    ],
    "featureRefs": [
      "F15",
      "F31",
      "F07",
      "B04",
      "B14",
      "B22"
    ],
    "acceptanceIds": [
      "A09",
      "A10",
      "A17",
      "A18",
      "A19",
      "A20",
      "A38"
    ],
    "specPage": 76,
    "currentStatus": "partial",
    "statusNote": "Trusted dispatch/canonicalization boundaries exist; no injection-boundary review UI."
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
    "catalogScreen": "S15",
    "defaultState": "Current served tools only",
    "scope": "Daemon/preset",
    "applyTiming": "On tool discovery",
    "behavior": "Show namespace, description, input schema, effect class and availability reason. An enabled tool can still be denied by active policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 77,
    "currentStatus": "partial",
    "statusNote": "Tool calls/catalog are visible in runs; no unified tool catalog screen."
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
    "catalogScreen": "S15",
    "defaultState": "Inherited effective set",
    "scope": "Preset/task",
    "applyTiming": "After toolset ack",
    "behavior": "Changing visibility never broadens grants. Disabled tools stop being offered on future calls; in-flight calls remain listed and separately cancellable.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 77,
    "currentStatus": "missing",
    "statusNote": "No per-tool enablement control."
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
    "catalogScreen": "S15",
    "defaultState": "No added server",
    "scope": "Machine/daemon trusted scope",
    "applyTiming": "After validation and supervised start",
    "behavior": "Match current bounds: at most 8 servers; name 128 bytes, command 512 bytes, 32 args of 512 bytes. Native CRUD is still required.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "mcp[]"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A21"
    ],
    "specPage": 77,
    "currentStatus": "partial",
    "statusNote": "Stdio MCP entries are config-only; no CRUD/start UI."
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
      "S16"
    ],
    "catalogScreen": "S15",
    "defaultState": "Unavailable in reviewed McpEntry config",
    "scope": "Connection/operator",
    "applyTiming": "After new transport capability",
    "behavior": "Do not display current stdio config as an HTTP/OAuth implementation. Validate destination, authentication and reconnect semantics before enabling.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "mcp[].transport (proposed)"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A22"
    ],
    "specPage": 77,
    "currentStatus": "missing",
    "statusNote": "McpEntry is stdio only; remote transport/OAuth is a new contract."
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
    "catalogScreen": "S15",
    "defaultState": "No inherited secret expansion",
    "scope": "Machine/server",
    "applyTiming": "Next supervised start",
    "behavior": "A new configuration/handoff contract must define allowed environment values. Never paste a bearer token into a shared project JSON field.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "mcp[].env (proposed)"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 78,
    "currentStatus": "missing",
    "statusNote": "No environment/secret-reference UI for MCP."
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
      "S16"
    ],
    "catalogScreen": "S15",
    "defaultState": "Signed out",
    "scope": "Machine/server account",
    "applyTiming": "After auth callback/revoke",
    "behavior": "Show account and server binding. Cancellation preserves the draft; failed auth does not become Connected. Current reviewed MCP config has no OAuth fields.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "mcp[].oauth (proposed)"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A22"
    ],
    "specPage": 78,
    "currentStatus": "missing",
    "statusNote": "No MCP OAuth lifecycle."
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
    "catalogScreen": "S15",
    "defaultState": "Observed state or unavailable",
    "scope": "Server instance",
    "applyTiming": "On heartbeat/observation",
    "behavior": "Show last success and bounded error detail. Installed does not mean connected, and connection does not certify tool behavior.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 78,
    "currentStatus": "partial",
    "statusNote": "Server config exists; health is not a first-class UI state."
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
    "catalogScreen": "S15",
    "defaultState": "Inherited policy; ask for effects",
    "scope": "Rule/preset",
    "applyTiming": "At dispatch",
    "behavior": "Use server instance plus stable tool name, not a display label. Newly discovered effectful tools do not inherit a careless wildcard from another server.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "permission-rules (proposed)"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 78,
    "currentStatus": "partial",
    "statusNote": "Tool dispatch resolves policy; no per-MCP-tool permission editor."
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
      "S15",
      "S22"
    ],
    "catalogScreen": "S15",
    "defaultState": "Collapsed inline summary",
    "scope": "Call",
    "applyTiming": "Before dispatch/after result",
    "behavior": "Redact secrets but show the exact target and material changes. Arguments changed after approval require a new authority decision.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 78,
    "currentStatus": "partial",
    "statusNote": "Tool call details are visible; no argument inspector/redaction view."
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
    "catalogScreen": "S15",
    "defaultState": "Effective runtime bounds",
    "scope": "Tool/server policy",
    "applyTiming": "Next operation",
    "behavior": "Show truncation and full artifact reference. A timeout marks whether an external effect is unknown; retry is not automatic for effectful tools.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [
      "tool timeout/output caps"
    ],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 78,
    "currentStatus": "partial",
    "statusNote": "Timeout/output bounds are enforced; no artifact-reference/uncertain-effect detail surface."
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
    "catalogScreen": "S15",
    "defaultState": "Effective runtime strategy",
    "scope": "Session/preset",
    "applyTiming": "Next context compile",
    "behavior": "Show definition/token cost when measurable. Installing many tools must not quietly consume the whole context window; unsupported deferred loading is explicit.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 79,
    "currentStatus": "partial",
    "statusNote": "Lazy tool exposure/definition loading is implemented in the backend; no toolset token-cost UI."
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
    "catalogScreen": "S15",
    "defaultState": "None selected",
    "scope": "Session/draft",
    "applyTiming": "On explicit read/invoke",
    "behavior": "Resources, templates, prompts and tools have different lifecycles. Treat server-supplied instructions as attributed content, not unconditional policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 79,
    "currentStatus": "missing",
    "statusNote": "No MCP resources/prompts browser."
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
    "catalogScreen": "S15",
    "defaultState": "Keep current server until action",
    "scope": "Server instance",
    "applyTiming": "After owner ack",
    "behavior": "Restart reconciles pending calls and process ownership. Removing a config never turns an unknown remote effect into cancelled.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 79,
    "currentStatus": "missing",
    "statusNote": "No MCP restart/logs/remove controls."
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
    "catalogScreen": "S15",
    "defaultState": "Not executable merely from repo config",
    "scope": "Project/operator",
    "applyTiming": "After explicit trusted installation",
    "behavior": "A checked-out repository cannot silently run arbitrary stdio servers. Show executable provenance and which user/machine granted the start authority.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 79,
    "currentStatus": "missing",
    "statusNote": "No shared/project server trust review."
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
    "catalogScreen": "S15",
    "defaultState": "Exclude credential values",
    "scope": "User/server",
    "applyTiming": "On explicit export",
    "behavior": "Include versions, toolset hash and bounded failure context. Copying diagnostics must never export environment secrets or private tool-result payloads by default.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Bounded stdio CRUD, health/discovery, toolset identity; separate negotiated remote transport/OAuth contract (p.030, p.042)."
    ],
    "runtimeEndpoints": [
      "GET /native/providers",
      "(no MCP CRUD route)"
    ],
    "featureRefs": [
      "F12",
      "F07",
      "F38",
      "B09",
      "B15",
      "B28"
    ],
    "acceptanceIds": [
      "A06",
      "A21",
      "A22",
      "A23",
      "A28"
    ],
    "specPage": 80,
    "currentStatus": "missing",
    "statusNote": "No toolset diagnostics export."
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
    "catalogScreen": "S17",
    "defaultState": "No fabricated marketplace inventory",
    "scope": "User/project/preset",
    "applyTiming": "On discovery",
    "behavior": "Show source, version, description, invocation policy and required capabilities. Current source review does not establish a complete skill-management API.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 81,
    "currentStatus": "partial",
    "statusNote": "Skill discovery exists (.faktor/skills, .claude/skills); no library UI."
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
    "catalogScreen": "S17",
    "defaultState": "Proposed: manual for effectful workflows",
    "scope": "Skill/preset",
    "applyTiming": "Next invocation",
    "behavior": "Visibility and authority are distinct. A hidden skill must not remain silently invocable through a guessed command alias.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 81,
    "currentStatus": "missing",
    "statusNote": "No invocation-policy control."
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
    "catalogScreen": "S17",
    "defaultState": "Read-only before installation",
    "scope": "Skill source",
    "applyTiming": "On open",
    "behavior": "Show exactly which instructions and scripts would load. Do not run setup or install dependencies merely to display a package.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 81,
    "currentStatus": "missing",
    "statusNote": "No skill details/install/update/remove flow."
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
    "catalogScreen": "S17",
    "defaultState": "No silent project installation",
    "scope": "User/project trusted store",
    "applyTiming": "After validated install ack",
    "behavior": "Keep prior known-good version on failed update. Removing a skill removes future discovery without pretending to erase earlier context copies.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 81,
    "currentStatus": "missing",
    "statusNote": "No skill details/install/update/remove flow."
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
    "catalogScreen": "S17",
    "defaultState": "Documented available commands",
    "scope": "Host/session",
    "applyTiming": "At invocation",
    "behavior": "Commands insert or act according to a visible label. Proposed names in this document are UX routes, not existing CLI commands.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "missing",
    "statusNote": "No slash-command palette; /source on is a tool toggle, not a general command registry."
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
    "catalogScreen": "S17",
    "defaultState": "None",
    "scope": "User/project",
    "applyTiming": "On explicit invocation",
    "behavior": "Validate arguments and show whether a command is prompt-only or executable. Do not interpret pasted command-looking text as an automatic shell action.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "missing",
    "statusNote": "No custom commands/templates."
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
    "catalogScreen": "S17",
    "defaultState": "No new hook installed",
    "scope": "Daemon/approved package",
    "applyTiming": "On config apply",
    "behavior": "Use exact native HookEvent names; do not copy Claude-specific events into Faktor config. Expose only events the running daemon supports.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].events (env FAKTOR_HOOKS)"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
    "catalogScreen": "S17",
    "defaultState": "User supplied",
    "scope": "Machine/trusted hook spec",
    "applyTiming": "After validation before spawn",
    "behavior": "Show executable host, working directory and version/source. A project hook cannot silently replace the machine runtime or expand its capability set.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].command/args"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
    "catalogScreen": "S17",
    "defaultState": "Event set only unless extended",
    "scope": "Hook spec",
    "applyTiming": "Next matching event",
    "behavior": "Preview matching events. A richer matcher is a new schema feature if the current HookSpec cannot express it.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "missing",
    "statusNote": "Current HookSpec has no rich match filter."
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
    "catalogScreen": "S17",
    "defaultState": "Native allowlist true",
    "scope": "Machine/hook spec",
    "applyTiming": "Next hook run",
    "behavior": "Show benign passthrough and explicit entries. Changing the switch must not dump the entire daemon environment into the child process.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].env_allowlist"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 82,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
    "catalogScreen": "S17",
    "defaultState": "Effective native hook spec",
    "scope": "Hook spec",
    "applyTiming": "Next run",
    "behavior": "On expiry, owned process tree is stopped and partial output cannot masquerade as an Allow verdict. Preserve a bounded audit excerpt.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].deadline_ms/stdout_cap/stderr_cap"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 83,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
      "S18"
    ],
    "catalogScreen": "S17",
    "defaultState": "Explicit per-hook policy",
    "scope": "Operator/hook",
    "applyTiming": "Next run",
    "behavior": "Explain what an operation will do after failure. FailOpen is a policy choice, not an error swallowed by the UI; managed constraints still apply.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].failure_policy"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 83,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
      "S18"
    ],
    "catalogScreen": "S17",
    "defaultState": "No production event triggered by preview",
    "scope": "Hook/test session",
    "applyTiming": "After explicit test admission",
    "behavior": "Test under the real capability/deadline envelope. Show Allow/Deny/Warn/metadata changes, exit state and exact tested version.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [
      "hooks[].test (proposed)"
    ],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A24"
    ],
    "specPage": 83,
    "currentStatus": "partial",
    "statusNote": "HookSpec/registry implements events, executable/args, env allowlist, deadlines/caps, failure policy and bounded audit; no editor/test route."
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
    "catalogScreen": "S17",
    "defaultState": "Untrusted until reviewed",
    "scope": "User/project/operator",
    "applyTiming": "Before enabling executable content",
    "behavior": "Keep rule/skill/hook installation distinct from permission grants. Lock managed versions where policy requires and explain unsigned/unverified provenance honestly.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK03",
      "BK06"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Skill/preset CRUD with trusted origin/version/invocation; hook management/test over native HookSpec (p.030, p.043)."
    ],
    "runtimeEndpoints": [
      "(no skill/hook management route)"
    ],
    "featureRefs": [
      "F13",
      "F33",
      "B19",
      "B20"
    ],
    "acceptanceIds": [
      "A06",
      "A10",
      "A23",
      "A24"
    ],
    "specPage": 83,
    "currentStatus": "missing",
    "statusNote": "No packaged customization trust review."
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
    "catalogScreen": "S19",
    "defaultState": "Existing native process view",
    "scope": "Session/task/agent",
    "applyTiming": "On event/snapshot",
    "behavior": "Show owner, working directory, age, process state and last output. A PID without a live owned handle is stale, not proof that the process is controllable.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
    "acceptanceIds": [
      "A27"
    ],
    "specPage": 84,
    "currentStatus": "exists",
    "statusNote": "Owned terminal process list, cwd, bounded output, background, input/kill/reconcile are implemented."
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
    "catalogScreen": "S19",
    "defaultState": "Daemon policy and candidate cwd",
    "scope": "Machine/task",
    "applyTiming": "Before spawn",
    "behavior": "Show which machine/container owns the shell. Do not use a global current directory or run in the owner checkout because the editor selection changed.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [
      "sandbox.shell",
      "sandbox.home"
    ],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 84,
    "currentStatus": "exists",
    "statusNote": "Owned terminal process list, cwd, bounded output, background, input/kill/reconcile are implemented."
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
    "catalogScreen": "S19",
    "defaultState": "Current bounded service view",
    "scope": "Process/output artifact",
    "applyTiming": "On output page",
    "behavior": "Show truncation, byte count and cursor. Do not append unlimited logs to the chat DOM or model context.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 84,
    "currentStatus": "exists",
    "statusNote": "Owned terminal process list, cwd, bounded output, background, input/kill/reconcile are implemented."
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
    "catalogScreen": "S19",
    "defaultState": "Foreground unless configured/selected",
    "scope": "Process/task",
    "applyTiming": "After background ack",
    "behavior": "Clarify that closing a UI tab does not terminate an owned process. Show dependencies and whether the parent waits for the result.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 84,
    "currentStatus": "exists",
    "statusNote": "Owned terminal process list, cwd, bounded output, background, input/kill/reconcile are implemented."
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
      "S19"
    ],
    "catalogScreen": "S19",
    "defaultState": "No action without valid owner",
    "scope": "Process/session",
    "applyTiming": "After exact lifecycle ack",
    "behavior": "Keep terminal kill, run cancellation and session abort separate. Unknown process effect requires reconciliation, not a blind restart.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
    "acceptanceIds": [
      "A27"
    ],
    "specPage": 85,
    "currentStatus": "exists",
    "statusNote": "Owned terminal process list, cwd, bounded output, background, input/kill/reconcile are implemented."
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
    "catalogScreen": "S19",
    "defaultState": "Unavailable until advertised",
    "scope": "Machine/session",
    "applyTiming": "After explicit connect",
    "behavior": "Browser automation needs a real tool adapter and permission envelope. Show local/remote browser and session scope; no product promise from a mock preview.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 85,
    "currentStatus": "missing",
    "statusNote": "No browser adapter route/UI, URL scope, screenshot capture or console/network observation surface."
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
    "catalogScreen": "S19",
    "defaultState": "No automatic arbitrary navigation",
    "scope": "Task/browser",
    "applyTiming": "At navigation admission",
    "behavior": "Expose target host and credentials/account context. Do not equate a successfully opened page with verified application behavior.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 85,
    "currentStatus": "missing",
    "statusNote": "No browser adapter route/UI, URL scope, screenshot capture or console/network observation surface."
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
    "catalogScreen": "S19",
    "defaultState": "No automatic cross-context sharing",
    "scope": "Task/artifact",
    "applyTiming": "After capture",
    "behavior": "Show origin and capture time; control whether screenshots enter model context. Keep secret-bearing pages out of shared exports by default.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 85,
    "currentStatus": "missing",
    "statusNote": "No browser adapter route/UI, URL scope, screenshot capture or console/network observation surface."
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
    "catalogScreen": "S19",
    "defaultState": "Unavailable without adapter events",
    "scope": "Task/browser",
    "applyTiming": "On observation",
    "behavior": "A clean console is one observation, not total correctness. Filter by severity while preserving the recorded underlying results.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 86,
    "currentStatus": "missing",
    "statusNote": "No browser adapter route/UI, URL scope, screenshot capture or console/network observation surface."
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
    "catalogScreen": "S19",
    "defaultState": "Current host diagnostics when included",
    "scope": "Workspace/draft/task",
    "applyTiming": "On explicit context capture",
    "behavior": "Show file, range and diagnostic version. Stale diagnostics are not evidence that the current candidate fails or passes.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 86,
    "currentStatus": "partial",
    "statusNote": "LSP diagnostics exist in host/editor; Faktor only captures selected diagnostics."
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
    "catalogScreen": "S19",
    "defaultState": "No action automatically selected",
    "scope": "Editor selection",
    "applyTiming": "On explicit action",
    "behavior": "Capture file URI, range and version. Fix opens a bounded task draft; it does not silently edit or overwrite a running conversation.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 86,
    "currentStatus": "missing",
    "statusNote": "No inline code-action integration."
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
    "catalogScreen": "S19",
    "defaultState": "Effective permission policy",
    "scope": "Operation",
    "applyTiming": "Before execution",
    "behavior": "Keep secret values masked but material executable/arguments visible. Show sandbox guarantees and whether output will be attached to the model.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [
      "sandbox.shell"
    ],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 86,
    "currentStatus": "exists",
    "statusNote": "Terminal command approval uses the same permission boundary."
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
      "S20",
      "S31"
    ],
    "catalogScreen": "S19",
    "defaultState": "Actual active host binding",
    "scope": "Workspace/session",
    "applyTiming": "On binding/reconnect",
    "behavior": "A local URL resolves on the execution host. Enforce URI/path mapping and invalidate stale handles after a remote-host change without losing drafts.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK05",
      "BK06",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Owned terminal/process lifecycle; browser adapters and URL/network scope are new contracts (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/terminals",
      "GET /native/session/{id}/terminal",
      "GET /native/session/{id}/terminal/events",
      "GET /native/session/{id}/terminals/{id}/output",
      "POST /native/session/{id}/terminal"
    ],
    "featureRefs": [
      "F26",
      "F31",
      "F07",
      "B13"
    ],
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
    "specPage": 86,
    "currentStatus": "partial",
    "statusNote": "Workspace/daemon binding fences remote callbacks; no full SSH/WSL/container binding contract."
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
      "S01",
      "S21"
    ],
    "catalogScreen": "S21",
    "defaultState": "Existing 0 = no limit",
    "scope": "User default; per-run override",
    "applyTiming": "Captured at admission",
    "behavior": "Show No limit rather than raw zero in the normal UI. Reject decimals, negatives, overflow and malformed pasted numbers before posting; preserve the draft.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "faktor.budgetTokens",
      "max_tokens"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
    "acceptanceIds": [
      "A30"
    ],
    "specPage": 87,
    "currentStatus": "exists",
    "statusNote": "Token and micro-USD caps are captured on task admission."
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
      "S01",
      "S21"
    ],
    "catalogScreen": "S21",
    "defaultState": "Existing 0 = no limit",
    "scope": "User default; per-run override",
    "applyTiming": "Captured at admission",
    "behavior": "Convert exactly using decimal arithmetic; never binary floating-point rounding. Example: USD 5.00 = 5000000 micro-USD, not 5 tokens or 5000000 dollars.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "faktor.budgetCostMicro",
      "max_cost_micro"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
    "acceptanceIds": [
      "A30"
    ],
    "specPage": 87,
    "currentStatus": "exists",
    "statusNote": "Token and micro-USD caps are captured on task admission."
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
    "catalogScreen": "S21",
    "defaultState": "Effective backend policy",
    "scope": "Task",
    "applyTiming": "Before reservation/dispatch",
    "behavior": "Warn on configured threshold where supported. Unknown price cannot be assumed to fit; expose refusal or explicitly documented bounded pricing policy.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
    "acceptanceIds": [
      "A30",
      "A31"
    ],
    "specPage": 87,
    "currentStatus": "partial",
    "statusNote": "Admission refuses over-cap requests; warning/enforcement distinction is limited."
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
      "S21",
      "S22",
      "S23",
      "S27",
      "S32"
    ],
    "catalogScreen": "S21",
    "defaultState": "Only durable observed values",
    "scope": "Task/session",
    "applyTiming": "After ledger observation",
    "behavior": "Open reservations are not final charges. Provider-reported and computed spend are different fields; uncertainty survives retries and cancellation.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "usage.durable.reservations"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 87,
    "currentStatus": "partial",
    "statusNote": "Reservations/settled/uncertain are served in usage; no dedicated reservation display."
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
    "catalogScreen": "S21",
    "defaultState": "No silent active-run edit",
    "scope": "Task/policy",
    "applyTiming": "After durable revision ack",
    "behavior": "A lower cap cannot erase settled spend or existing reservations. Show when a new ceiling blocks future work; revised draft defaults do not alter captured tasks.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "task budget revision (proposed)"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 88,
    "currentStatus": "missing",
    "statusNote": "No active-task budget revision control."
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
    "catalogScreen": "S21",
    "defaultState": "Daemon 3 total attempts",
    "scope": "Daemon/operator",
    "applyTiming": "Next eligible request",
    "behavior": "Label total attempts, not retries, to avoid off-by-one cost expectations. State-aware replay must never repeat a request after durable effect/stream admission.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "retry.max_attempts"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 88,
    "currentStatus": "partial",
    "statusNote": "Total attempts, base/max delay and jitter are validated in config; no editor."
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
    "catalogScreen": "S21",
    "defaultState": "250 ms / 5000 ms / 0.2",
    "scope": "Daemon/operator",
    "applyTiming": "Next eligible request",
    "behavior": "Base 1..60000 ms; max >= base and <=120000 ms; jitter 0..1. Show retry class and countdown, with Stop available.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "retry.base_delay_ms",
      "retry.max_delay_ms",
      "retry.jitter"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 88,
    "currentStatus": "partial",
    "statusNote": "Total attempts, base/max delay and jitter are validated in config; no editor."
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
    "catalogScreen": "S21",
    "defaultState": "Effective retry policy",
    "scope": "Provider/operation",
    "applyTiming": "On typed limit",
    "behavior": "Distinguish provider rate limit, account quota and local queue saturation. Do not advertise a reset time that the provider did not supply.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 88,
    "currentStatus": "partial",
    "statusNote": "Typed retry classes exist; no rate-limit recovery panel."
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
    "catalogScreen": "S21",
    "defaultState": "Effective daemon bound",
    "scope": "Task/preset",
    "applyTiming": "At admission or policy ack",
    "behavior": "Expose actual loop guards without inventing an active max-turns knob. If not supported, show Planned and keep bounded backend behavior authoritative.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "agent loop/step bounds"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 89,
    "currentStatus": "partial",
    "statusNote": "Loop/step guards exist in orchestration; no served max-turns control."
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
    "catalogScreen": "S21",
    "defaultState": "Served owner deadline",
    "scope": "Task/operation",
    "applyTiming": "At admission",
    "behavior": "Show what expires: model call, tool, task or session. Timeout is not necessarily cancellation acknowledged or remote effect rolled back.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [
      "verification.quick_max_s",
      "verification.unit_max_s"
    ],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 89,
    "currentStatus": "partial",
    "statusNote": "Deadlines exist per verification/service; no task/operation deadline editor."
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
    "catalogScreen": "S21",
    "defaultState": "Keep coherent snapshot",
    "scope": "Connection/session",
    "applyTiming": "On classified error",
    "behavior": "401, 429, timeout, unavailable provider and stale daemon need different actions. No generic Retry button that can duplicate an admitted effectful task.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
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
    "specPage": 89,
    "currentStatus": "partial",
    "statusNote": "Connectivity/auth failures are rendered; no unified classified recovery panel."
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
      "S14",
      "S21",
      "S30"
    ],
    "catalogScreen": "S21",
    "defaultState": "Original immutable submission",
    "scope": "Prompt/task",
    "applyTiming": "After durable reconciliation",
    "behavior": "Same UUID with same captured body may recover its receipt; changing model, attachments or budget creates a different request and must not reuse that key.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Exact micro-USD ledger, shared parent/child reservations, idempotent settlement/refund (p.032, p.044)."
    ],
    "runtimeEndpoints": [
      "POST /native/session/{id}/task-runs",
      "GET /native/session/{id}/usage",
      "GET /native/usage"
    ],
    "featureRefs": [
      "F01",
      "F11",
      "F19",
      "F34",
      "B21"
    ],
    "acceptanceIds": [
      "A28",
      "A29"
    ],
    "specPage": 89,
    "currentStatus": "partial",
    "statusNote": "Idempotent admission/receipt exists; no uncertain-admission reconciliation UI."
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
      "S24"
    ],
    "catalogScreen": "S24",
    "defaultState": "Current immutable candidate",
    "scope": "Task/candidate",
    "applyTiming": "On served change basis",
    "behavior": "Preserve current native diff integration. Display base/candidate hashes and scope; editor navigation must never compare against a different session’s files.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
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
    "specPage": 90,
    "currentStatus": "exists",
    "statusNote": "Native change review/diff is implemented."
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
    "catalogScreen": "S24",
    "defaultState": "No edit from merely reading",
    "scope": "Review/task draft",
    "applyTiming": "On explicit submit",
    "behavior": "Tie comments to exact file and diff version. Stale comments prompt rebase or clarification; a follow-up task gets its own captured contract.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
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
    "specPage": 90,
    "currentStatus": "partial",
    "statusNote": "Review comments/next task are not a complete versioned review workflow."
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
      "S24"
    ],
    "catalogScreen": "S24",
    "defaultState": "Read-only review unless supported",
    "scope": "Candidate",
    "applyTiming": "After explicit supported action",
    "behavior": "Review marks do not integrate files. Partial landing needs new proof for the changed candidate; never treat old full-candidate evidence as proof for a cherry-picked subset.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
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
    "specPage": 90,
    "currentStatus": "missing",
    "statusNote": "No hunk/file acceptance or partial landing."
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
      "S25"
    ],
    "catalogScreen": "S24",
    "defaultState": "Only actual recorded checkpoints",
    "scope": "Session/candidate",
    "applyTiming": "On checkpoint read",
    "behavior": "Hash-only rows may lack content for redo/diff. Show unavailable rather than promising a universal undo; no checkpoint is invented from a completed message.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A33"
    ],
    "specPage": 90,
    "currentStatus": "partial",
    "statusNote": "Checkpoint rows are served; no timeline/coverage UI and no restore."
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
    "catalogScreen": "S24",
    "defaultState": "No destructive default selected",
    "scope": "Checkpoint/branch",
    "applyTiming": "Before a separate restore transaction",
    "behavior": "Show affected and untracked/uncovered files, external effects and unrelated user edits. Exact content preconditions must be enforced by the backend.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A33"
    ],
    "specPage": 91,
    "currentStatus": "missing",
    "statusNote": "No restore preview, fork or redo operation/UI."
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
    "catalogScreen": "S24",
    "defaultState": "Preserve current session",
    "scope": "Session/checkpoint",
    "applyTiming": "After durable fork/restore ack",
    "behavior": "Conversation branching and file restoration are different. Missing after blobs disable redo; a fork must not erase subsequent work or duplicate remote side effects.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A33",
      "A34"
    ],
    "specPage": 91,
    "currentStatus": "missing",
    "statusNote": "No restore preview, fork or redo operation/UI."
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
      "S01",
      "S10",
      "S24",
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "Existing per-task fields",
    "scope": "Task draft",
    "applyTiming": "Captured at admission",
    "behavior": "Keep required status and claim scope visible. Show a versioned contract; editing the next draft cannot change criteria that an active candidate was evaluated against.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "completion_contract"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A35"
    ],
    "specPage": 91,
    "currentStatus": "exists",
    "statusNote": "Acceptance criteria and task contracts are captured and rendered."
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
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "60 s quick / 600 s unit / full background",
    "scope": "Daemon/task policy",
    "applyTiming": "After supported config reload",
    "behavior": "quick_max_s=0 disables service and leaves mutating work Unverified. Advanced/optional provider availability is explicit; native checks remain independently useful.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "verification.quick_max_s",
      "verification.unit_max_s",
      "verification.full_as_background"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
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
    "specPage": 91,
    "currentStatus": "partial",
    "statusNote": "Verification policy exists in config; no editor."
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
      "S24",
      "S26",
      "S27",
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "Pending until all required facts exist",
    "scope": "Task/candidate",
    "applyTiming": "On durable proof/landing",
    "behavior": "Separate every boundary; missing required result stays pending. Session Completed or an empty passing required set cannot independently establish verified engineering success.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "completion proof"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A35",
      "A36"
    ],
    "specPage": 92,
    "currentStatus": "exists",
    "statusNote": "Completion basis/proof and verified-vs-landed identity are implemented."
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
      "S01",
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "Local",
    "scope": "Task draft",
    "applyTiming": "Captured at admission",
    "behavior": "Both current hosts now expose matching four levels. State implied earlier actions; selecting PR requires actual SCM configuration and does not bypass verification.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "completion level"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A35"
    ],
    "specPage": 92,
    "currentStatus": "exists",
    "statusNote": "Four finish levels are exposed in both hosts."
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
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "Effective configured target",
    "scope": "Task/operator",
    "applyTiming": "Before captured finish",
    "behavior": "Show repo, target branch and access scope. Missing canonical SCM is a blocker; a successful arbitrary shell command is not a native PR receipt.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "completion.scm (proposed)"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
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
    "specPage": 92,
    "currentStatus": "partial",
    "statusNote": "SCM target/privacy policy exists in config; no full target editor."
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
      "S25",
      "S32"
    ],
    "catalogScreen": "S24",
    "defaultState": "No overwrite of unrelated work",
    "scope": "Candidate/owner checkout",
    "applyTiming": "On compare-and-swap failure",
    "behavior": "Show changed basis and new verification obligations. Restoring or editing invalidates stale proof; retain the previous verified artifact as historical evidence.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK09"
    ],
    "configKeys": [
      "expected_current_hash"
    ],
    "runtimeRequired": [
      "Content-aware restore/fork + candidate/proof identity; finish receipts (p.032, p.046)."
    ],
    "runtimeEndpoints": [
      "GET /native/session/{id}/checkpoints",
      "GET /native/session/{id}/verification",
      "GET /native/tasks/{id}/proof",
      "GET /native/tasks/{id}/completion-steps"
    ],
    "featureRefs": [
      "F03",
      "F14",
      "F16",
      "F17",
      "F29",
      "B07",
      "B17"
    ],
    "acceptanceIds": [
      "A34"
    ],
    "specPage": 92,
    "currentStatus": "partial",
    "statusNote": "CAS/manual-edit refusal exists in review/restore paths; no conflict rebase UI."
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
      "S31"
    ],
    "catalogScreen": "S26",
    "defaultState": "Existing durable conversation",
    "scope": "Session/host",
    "applyTiming": "On deliberate switch",
    "behavior": "Preserve per-session draft, scroll anchor and tool view. Late callbacks from the previous session cannot overwrite the newly selected session.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "exists",
    "statusNote": "Conversation selector and per-session state exist."
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
      "S26"
    ],
    "catalogScreen": "S26",
    "defaultState": "Existing title filter",
    "scope": "Selected project/session list",
    "applyTiming": "On query",
    "behavior": "Show loaded/list scope and paging. Do not claim full-text conversation search from a title-only filter. Distinguish no matches from history unavailable.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "exists",
    "statusNote": "Durable title-filtered history exists."
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
      "S26"
    ],
    "catalogScreen": "S26",
    "defaultState": "Off",
    "scope": "Open conversation",
    "applyTiming": "On host action",
    "behavior": "Search the full paged transcript, including not-yet-rendered messages. A virtualized screen subset is not the full search universe.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03",
      "BK08"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "missing",
    "statusNote": "No full-transcript Find across unloaded pages."
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
    "catalogScreen": "S26",
    "defaultState": "Current state",
    "scope": "Session",
    "applyTiming": "After metadata ack",
    "behavior": "Pinning may be host-local if labelled. Rename/archive needs real metadata support; never just change the displayed text and lose it on refresh.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [
      "session metadata (proposed)"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "missing",
    "statusNote": "No rename/pin/archive metadata actions."
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
      "S01",
      "S22"
    ],
    "catalogScreen": "S26",
    "defaultState": "Do not interrupt implicitly",
    "scope": "Session/prompt queue",
    "applyTiming": "After admission or queue edit ack",
    "behavior": "Use existing prompt admission/queue semantics where available. Separate send next, interrupt then send, and a draft not yet admitted; show queue ownership and cap.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [
      "prompt queue"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "partial",
    "statusNote": "Queue/abort/steer exist for prompts/children; no full queue editor for the main run."
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
    "catalogScreen": "S26",
    "defaultState": "No deletion selected",
    "scope": "Session",
    "applyTiming": "After appropriate durable/host action",
    "behavior": "Export a bounded redacted transcript plus metadata. Delete states what records, artifacts and forks remain; a running task needs a real stop/retain decision.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [
      "session export/delete (proposed)"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 93,
    "currentStatus": "missing",
    "statusNote": "No fork; export/delete are not complete native actions."
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
      "S19",
      "S22",
      "S27"
    ],
    "catalogScreen": "S26",
    "defaultState": "Compact tool summaries",
    "scope": "Session/operation",
    "applyTiming": "On durable events",
    "behavior": "Show model calls, tool results, approvals, cost and changed-file badges. Preserve original timestamps and output artifacts; do not invent a percent-complete bar.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 94,
    "currentStatus": "exists",
    "statusNote": "Live tool timeline/durable events are rendered."
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
      "S22"
    ],
    "catalogScreen": "S26",
    "defaultState": "Proposed: balanced",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Collapsing tool detail does not hide pending approval, failures, missing proof or current task checklist. Expansion is local UI state, not new model context.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 94,
    "currentStatus": "exists",
    "statusNote": "Focus/verbose display states are rendered."
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
      "S29"
    ],
    "catalogScreen": "S26",
    "defaultState": "Host theme/font/zoom",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Respect VS Code tokens and JetBrains UIManager/JBUI metrics. Marketing fonts stay on document/website surfaces, not IDE reading controls.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [
      "host theme/zoom",
      "density (proposed)"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 94,
    "currentStatus": "partial",
    "statusNote": "Host theme/zoom are inherited; density is not a Faktor setting."
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
    "catalogScreen": "S26",
    "defaultState": "Current Ctrl/Cmd+Enter pattern",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Offer discoverable send settings without stealing standard Tab/Shift+Tab focus. IME composition and paste must never accidentally admit a task.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [
      "send keybinding"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 94,
    "currentStatus": "exists",
    "statusNote": "Composer send behavior (Ctrl/Cmd+Enter) is implemented."
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
      "S29"
    ],
    "catalogScreen": "S26",
    "defaultState": "Host keymap",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Show shortcuts in tooltips and menus; allow remap. Proposed /model, /context and /permissions are aliases only after implemented, not current command claims.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [
      "package.json commands/keybindings"
    ],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 95,
    "currentStatus": "partial",
    "statusNote": "Twelve native commands exist; no full command/keymap registry or /model,/context,/permissions aliases."
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
    "catalogScreen": "S26",
    "defaultState": "Local notices; no unsolicited external sends",
    "scope": "User",
    "applyTiming": "Immediate UI",
    "behavior": "Never cover the editor for routine progress. Pending questions name the child/session; alerts remain available to screen readers without announcing every streamed token.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 95,
    "currentStatus": "partial",
    "statusNote": "Approval/attention cards exist; no unified notification policy."
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
      "S30"
    ],
    "catalogScreen": "S26",
    "defaultState": "Existing host windowing/coalescing",
    "scope": "Host/session",
    "applyTiming": "On render/page",
    "behavior": "Preserve follow-tail versus manual scroll. Recover offscreen matches through paging; do not drop durable events to make animation look smoother.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A40"
    ],
    "specPage": 95,
    "currentStatus": "exists",
    "statusNote": "Windowed/coalesced transcript and bounded rendering are implemented and tested."
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
      "S29"
    ],
    "catalogScreen": "S26",
    "defaultState": "Host accessibility preferences",
    "scope": "User/host",
    "applyTiming": "Immediate UI",
    "behavior": "Keyboard reaches every control; visible focus and labels survive 240 px panels and 200% text. Use names/shapes with color, editable plain-text tables and deliberate live-region summaries.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "H",
    "buildPackages": [
      "UI01",
      "UI02",
      "UI03"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A39"
    ],
    "specPage": 95,
    "currentStatus": "partial",
    "statusNote": "Keyboard/accessibility checks exist; full 240px/200% screen-reader acceptance is not yet a release gate for all new screens."
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
      "S31"
    ],
    "catalogScreen": "S26",
    "defaultState": "Existing selected conversation; bounded proposed parallelism",
    "scope": "Host/session/worktree",
    "applyTiming": "After supported admission or switch ack",
    "behavior": "Show which worktree, daemon and run owns each tab. A tab switch does not stop work; creating another active task requires explicit capacity and budget admission. Never share drafts or changed-file identities across worktrees.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK08",
      "BK09"
    ],
    "configKeys": [],
    "runtimeRequired": [
      "Durable session metadata, queue/search/fork operations with generation fencing (p.031, p.045)."
    ],
    "runtimeEndpoints": [
      "GET /native/sessions",
      "GET /native/messages",
      "GET /native/events"
    ],
    "featureRefs": [
      "F02",
      "F04",
      "F24",
      "F25",
      "F32",
      "B08",
      "B13",
      "B23",
      "B25"
    ],
    "acceptanceIds": [
      "A27",
      "A29",
      "A38",
      "A39",
      "A40"
    ],
    "specPage": 96,
    "currentStatus": "missing",
    "statusNote": "No parallel conversations/worktree tabs."
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
      "S29"
    ],
    "catalogScreen": "S29",
    "defaultState": "All effective readable values",
    "scope": "Allowed scopes only",
    "applyTiming": "On snapshot refresh",
    "behavior": "Each row shows label, source, scope, timing, reset and lock. Hide secrets; compare changes before save and keep invalid edits without partial persistence.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "effective settings search (proposed)"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 97,
    "currentStatus": "missing",
    "statusNote": "No effective-settings search across scopes."
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
    "catalogScreen": "S29",
    "defaultState": "Actual configured route",
    "scope": "Task/connection",
    "applyTiming": "Before capture and route change",
    "behavior": "Local to the service is not automatically local to the user. A hosted fallback must show its data boundary; turning cloud features off does not disable legitimate local work.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "route/egress policy"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 97,
    "currentStatus": "partial",
    "statusNote": "Local/hosted route and embedding status are partially visible; no unified data-boundary panel."
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
    "catalogScreen": "S29",
    "defaultState": "Local save only",
    "scope": "User/session",
    "applyTiming": "On explicit action",
    "behavior": "Include versions, route/capability state, ownership and bounded errors; exclude credential values, source payloads and private screenshots unless deliberately selected.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "diagnostic export (proposed)"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 97,
    "currentStatus": "partial",
    "statusNote": "Diagnostics exist; no preview/redacted export bundle UI."
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
      "S29",
      "S30"
    ],
    "catalogScreen": "S29",
    "defaultState": "Current discovery policy",
    "scope": "Machine/application only",
    "applyTiming": "At explicit service start",
    "behavior": "Project settings cannot change the executable path. Recorded checksum match is not signature verification; mismatch refuses start and unrecorded paths need exact-path confirmation.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "faktor.binaryPath",
      "faktor.dataDir",
      "faktor.installRoot"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 97,
    "currentStatus": "exists",
    "statusNote": "Runtime path/checksum/readiness and start/stop commands exist."
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
      "S30"
    ],
    "catalogScreen": "S29",
    "defaultState": "No start from mere view opening",
    "scope": "Machine/daemon",
    "applyTiming": "After lifecycle ack",
    "behavior": "Show starting, ready, incompatible, stopped and refused separately. Signed updater availability is conditional; an update action must preserve active-run ownership and rollback state.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "updater.*"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 98,
    "currentStatus": "exists",
    "statusNote": "Start/stop/update lifecycle commands and updater routes exist."
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
      "S27",
      "S32"
    ],
    "catalogScreen": "S29",
    "defaultState": "Unavailable rather than zero",
    "scope": "Local/session/org when enabled",
    "applyTiming": "On served usage/account read",
    "behavior": "Separate API charges, local operating estimates, subscription quota and reservations. Provider quotas are only shown when supplied; no fake balance on a local model.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "billing.*"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 98,
    "currentStatus": "exists",
    "statusNote": "Usage/pricing/account/outcomes views exist."
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
      "S29",
      "S30",
      "S31"
    ],
    "catalogScreen": "S29",
    "defaultState": "Disabled/empty endpoint",
    "scope": "Machine/user account",
    "applyTiming": "After verified auth/revoke",
    "behavior": "VS Code has a guided path; JetBrains currently includes manual credential storage. Removing a local stored credential is distinct from revoking a remote session.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "E",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "faktor.controlPlaneEndpoint",
      "faktor.controlPlaneOrganization"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 98,
    "currentStatus": "exists",
    "statusNote": "Control-plane sign-in/out and credential storage exist (JetBrains manual credential path)."
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
    "catalogScreen": "S29",
    "defaultState": "Disabled when enterprise not enabled",
    "scope": "Organization/operator",
    "applyTiming": "After effective policy ack",
    "behavior": "Reuse current enterprise effective-config concepts only when enabled. Do not imply arbitrary local preference keys are already enforced by that service.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "enterprise.*"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 98,
    "currentStatus": "partial",
    "statusNote": "Enterprise managed config exists; no lock/provenance UI."
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
    "catalogScreen": "S29",
    "defaultState": "Current stored retention policy",
    "scope": "User/operator",
    "applyTiming": "After acknowledged policy/job",
    "behavior": "Show sessions, snapshots, artifacts, logs and learned facts separately. Never delete active-owned records or silently equate hiding chat with deleting retained files.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "retention policy (proposed)"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 99,
    "currentStatus": "missing",
    "statusNote": "No retention/cleanup policy UI or dry-run list."
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
    "catalogScreen": "S29",
    "defaultState": "Proposed: no content telemetry by default",
    "scope": "User/managed policy",
    "applyTiming": "After acknowledged choice",
    "behavior": "Local UX counters and uploaded content are different. Opt-in/out must show endpoint and scope; no unsupported compliance or confidentiality badge.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "telemetry (proposed)"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 99,
    "currentStatus": "missing",
    "statusNote": "No telemetry/feedback settings."
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
    "catalogScreen": "S29",
    "defaultState": "Local unless configured",
    "scope": "Task/operator",
    "applyTiming": "At placement admission",
    "behavior": "Workers are optional. Show job generation, machine, lease and egress boundary; the IDE cannot invent a remote capacity or silently move local work to cloud.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Disabled only when capability/authority is unavailable; the missing prerequisite is explained (p.007, p.013).",
    "classification": "R",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "workers.*"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 99,
    "currentStatus": "partial",
    "statusNote": "Worker config/routes exist; no placement UI."
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
    "catalogScreen": "S29",
    "defaultState": "No secrets or executable auto-start",
    "scope": "User/operator",
    "applyTiming": "After preview and apply",
    "behavior": "Report unsupported keys, policy conflicts, required restart and missing secret references. Never import machine executable paths from an untrusted project.",
    "emptyState": "Empty collection names the add/discover/refresh action; no placeholder values (p.013).",
    "errorState": "Displays the typed failure/refusal and safe recovery; preserves draft and last coherent state (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "config import/export (proposed)"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 99,
    "currentStatus": "missing",
    "statusNote": "No configuration import/export."
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
    "catalogScreen": "S29",
    "defaultState": "Actual daemon and host support",
    "scope": "Daemon/host",
    "applyTiming": "On version/capability handshake",
    "behavior": "Keep unavailable controls explainable and relevant, not a sea of inert toggles. The design catalog is complete; shipped menus include only backed capabilities.",
    "emptyState": "Shows the specified None/No/Unavailable state with the safe next action; never a fabricated value (p.013).",
    "errorState": "Typed cause, affected scope and safe recovery; invalid input is preserved (p.013).",
    "disabledState": "Shows the specified unavailable/unsupported/locked/read-only state with the missing prerequisite; no inert toggle (p.007, p.013).",
    "classification": "N",
    "buildPackages": [
      "BK01",
      "UI01",
      "UI02",
      "UI03",
      "BK07",
      "BK10"
    ],
    "configKeys": [
      "version/capability handshake"
    ],
    "runtimeRequired": [
      "Versioned effective settings, diagnostics/allowlisted export, runtime trust and optional services (p.027, p.047)."
    ],
    "runtimeEndpoints": [
      "GET /native/health",
      "GET /native/ready",
      "GET /native/index/coverage",
      "GET /native/semantic/status",
      "GET /native/updater/status"
    ],
    "featureRefs": [
      "F01",
      "F21",
      "F22",
      "F30",
      "F35",
      "F36",
      "B11",
      "B26"
    ],
    "acceptanceIds": [
      "A06",
      "A11",
      "A16",
      "A31",
      "A36",
      "A37",
      "A38"
    ],
    "specPage": 100,
    "currentStatus": "partial",
    "statusNote": "Capability handshake/introspection exists; no feature-status screen."
  }
];

export const E5_SCREENS: readonly Edition5Screen[] = [
  {
    "id": "S01",
    "title": "Work: a complete agent cockpit",
    "purpose": "Compact cockpit exposes intent, model, effort, authority, context, caps and pending attention beside the draft; Send captures an immutable configuration summary and task contract.",
    "action": "Open Task options without losing the draft; Send captures an immutable configuration summary and task contract.",
    "recovery": "While disconnected, retain the draft and last coherent transcript; mark observations stale and disable admission until the session is reconciled.",
    "controls": [
      "A-01",
      "M-01",
      "M-02",
      "M-04",
      "G-01",
      "C-01",
      "Q-01",
      "B-01",
      "B-02",
      "D-05",
      "D-07",
      "V-07",
      "V-10"
    ],
    "layoutRegions": "Identity/header (task, model, effort, authority, context, caps), transcript/body, attention rail, composer, status row.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: While disconnected, retain the draft and last coherent transcript; mark observations stale and disable admission until the session is reconciled.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W03, W04, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.101; plates pp.109–110; controls `A-01 M-01 M-02 M-04 G-01 C-01 Q-01 B-01 B-02 D-05 D-07 V-07 V-10`."
  },
  {
    "id": "S02",
    "title": "Model picker and capability details",
    "purpose": "Search by exact ID, provider or capability; expose authority, limits and availability before selection; favorites/recent are user-local; show New conversation when the current session cannot change safely.",
    "action": "Choose a served model and commit its provider/model pair; show New conversation when this session cannot change safely.",
    "recovery": "An unknown or unavailable model remains visible with a reason; do not silently substitute it; a blocked tool capability prevents tool-using admission.",
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
    "layoutRegions": "Search field, pinned/favorites/recent groups, served-model list with authority/limits/availability, selected-model detail, select/cancel row.",
    "vscodeVariant": "QuickPick-style host picker over Work; Enter selects, arrows navigate, Esc cancels and preserves the previous pair/draft; Tab keeps normal focus.",
    "jetbrainsVariant": "Native dialog/list picker over the tool window; Enter selects, Esc cancels; the same requested-vs-actual and unavailable labels.",
    "visual": "32 px rows, 8 px item gap, full-width search, selected-detail pane; 240 px vertical stack; Enter/Esc/Tab use host focus rules.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: An unknown or unavailable model remains visible with a reason; do not silently substitute it; a blocked tool capability prevents tool-using admission.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W01, W03. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.101; plates pp.111–112; controls `M-01 M-02 M-03 M-04 M-05 M-06 M-07 M-08 M-09 M-10 M-11 M-12 M-13 M-14`."
  },
  {
    "id": "S03",
    "title": "Provider connection editor",
    "purpose": "Connections have a stable instance ID, API family, endpoint and credential reference; test auth, model discovery and capability support as separate stages; save only after secure binding.",
    "action": "Validate a staged draft, preview the diff and save only after secure binding succeeds; keep the last valid connection when validation fails.",
    "recovery": "A 401 offers Rotate credential; blocked destination shows endpoint policy; discovery failure keeps manually declared model IDs intact.",
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
    "layoutRegions": "Connection list, identity/API family, endpoint and credential reference, staged test stages (reachability/auth/catalog/capability), diff/apply footer.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A 401 offers Rotate credential; blocked destination shows endpoint policy; discovery failure keeps manually declared model IDs intact.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W01. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.101; plates pp.113–114; controls `P-01 P-02 P-03 P-04 P-05 P-06 P-10 P-11 P-12 P-14`."
  },
  {
    "id": "S04",
    "title": "Local model setup and runtime allocation",
    "purpose": "Locality is relative to the daemon; explain loopback, allocation, model readiness and resource ownership before offering local-runtime controls.",
    "action": "Test the exact endpoint from the daemon host, then show configured and observed allocation side by side.",
    "recovery": "A private LAN endpoint receives a typed refusal; allow_loopback cannot authorize it; unsupported unload/allocation controls stay unavailable.",
    "controls": [
      "P-07",
      "P-08",
      "P-09",
      "P-13",
      "P-15",
      "C-10",
      "M-09"
    ],
    "layoutRegions": "Execution location, local adapter, endpoint/test, declared vs observed allocation, embedding boundary, context preview.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A private LAN endpoint receives a typed refusal; allow_loopback cannot authorize it; unsupported unload/allocation controls stay unavailable.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W02. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.101; plates pp.115–116; controls `P-07 P-08 P-09 P-13 P-15 C-10 M-09`."
  },
  {
    "id": "S05",
    "title": "Model tuning: reasoning and generation",
    "purpose": "Capability-driven form exposes standard generation controls without pretending every provider supports every field; Unset is distinct from zero; Preview shows the normalized request and apply timing.",
    "action": "Reset removes an override; Preview shows the normalized request and apply timing; changes apply only after a supported acknowledgement.",
    "recovery": "Unsupported values fail locally before admission; changing the model revalidates the complete tuning bundle and retains incompatible edits for review.",
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
    "layoutRegions": "Model identity, reasoning, thinking budget, sampling, output limit, advanced fields, reset/preview/apply bar.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unsupported values fail locally before admission; changing the model revalidates the complete tuning bundle and retains incompatible edits for review.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W01, W03. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.102; plates pp.117–118; controls `G-01 G-02 G-03 G-04 G-05 G-06 G-07 G-08 G-09 G-10 G-11 G-12 G-13 G-14`."
  },
  {
    "id": "S06",
    "title": "Agent presets and execution envelopes",
    "purpose": "Code, Ask, Plan, Debug and Review are proposed task envelopes with visible instructions, models, tool limits and delegation policy; a preset cannot override managed denies.",
    "action": "Select or duplicate a preset, then inspect resolved permissions before applying it to a new task.",
    "recovery": "A prompt-only Ask route is labelled accordingly until read-only dispatch is enforced; a preset cannot override managed denies.",
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
    "layoutRegions": "Preset library, preset editor, resolved permissions/envelope, duplicate/apply actions.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A prompt-only Ask route is labelled accordingly until read-only dispatch is enforced; a preset cannot override managed denies.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W04. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.102; plates pp.119–120; controls `A-01 A-02 A-03 A-04 A-05 A-06 A-07 A-08 A-09 A-10 A-11 A-12`."
  },
  {
    "id": "S07",
    "title": "Custom agent and delegation settings",
    "purpose": "Build a role from bounded instructions, model settings, tools and parent-owned budget; preview the resulting envelope before saving; active children retain their admitted version.",
    "action": "Save a validated version for the next task; an active child retains its admitted version until a supported control changes it.",
    "recovery": "Duplicate names do not collide with stable IDs; invalid capability/model combinations stay in the draft and cannot be applied.",
    "controls": [
      "A-04",
      "A-05",
      "A-06",
      "A-07",
      "A-08",
      "A-09",
      "A-11",
      "A-12",
      "M-05"
    ],
    "layoutRegions": "Role/instructions, model/reasoning, tool allowlist, delegation limits, parent budget, envelope preview.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Duplicate names do not collide with stable IDs; invalid capability/model combinations stay in the draft and cannot be applied.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W04, W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.102; plates pp.121–122; controls `A-04 A-05 A-06 A-07 A-08 A-09 A-11 A-12 M-05`."
  },
  {
    "id": "S08",
    "title": "Context inspector and capacity accounting",
    "purpose": "Inspect what enters a call, its estimated/observed token cost and provenance; capacity and input usage have different bases; overflow must be resolved before sending.",
    "action": "Pin, exclude or open an attributed source; resolve overflow before sending; show the preview version admission will capture.",
    "recovery": "With current contextUsage=null, measured usage reads Unavailable; estimates remain useful but never masquerade as a durable runtime meter.",
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
    "layoutRegions": "Capacity totals, category accounting, source manifest, overflow actions, preview/capture bar.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: With current contextUsage=null, measured usage reads Unavailable; estimates remain useful but never masquerade as a durable runtime meter.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W02, W05, W06. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.102; plates pp.123–124; controls `C-01 C-02 C-04 C-05 C-07 C-08 C-09 C-10 C-11 C-12 C-13 C-14`."
  },
  {
    "id": "S09",
    "title": "Mentions, attachments and explicit context",
    "purpose": "One discoverable picker adds exact files, ranges, diagnostics, terminal excerpts, diffs, URLs and prior-chat references to a draft; failed items stay visible and removable.",
    "action": "Select a source to preview resolved URI, range, version, bytes and inclusion state; Escape closes the picker without clearing text.",
    "recovery": "Oversize, stale or unsupported attachments stay visible with a reason and Remove/Retry; do not silently omit a failed item on Send.",
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
    "layoutRegions": "Search/mention field, grouped sources (files, symbols, diagnostics, terminal, diffs, URLs, prior chats), source preview, selected attachment chips.",
    "vscodeVariant": "QuickPick-style host picker over Work; Enter selects, arrows navigate, Esc cancels and preserves the previous pair/draft; Tab keeps normal focus.",
    "jetbrainsVariant": "Native dialog/list picker over the tool window; Enter selects, Esc cancels; the same requested-vs-actual and unavailable labels.",
    "visual": "32 px rows, 8 px item gap, full-width search, selected-detail pane; 240 px vertical stack; Enter/Esc/Tab use host focus rules.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Oversize, stale or unsupported attachments stay visible with a reason and Remove/Retry; do not silently omit a failed item on Send.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W05. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.103; plates pp.125–126; controls `C-03 C-04 C-05 C-06 C-07 C-08 C-13 X-10 X-11`."
  },
  {
    "id": "S10",
    "title": "Compaction and continuity review",
    "purpose": "Compaction preserves task obligations and operation identity while reducing eligible history; show a plan before dispatch and a measured result afterwards.",
    "action": "Review retained facts and model/egress/cost implications, then Compact using a new durable operation contract.",
    "recovery": "Failure or insufficient reduction keeps the prior valid basis; pending approvals and unknown external effects cannot be erased by summarization.",
    "controls": [
      "K-01",
      "K-02",
      "K-03",
      "K-04",
      "C-12",
      "I-11",
      "V-07"
    ],
    "layoutRegions": "Freshness/basis, retained-facts manifest, summary model/egress/cost, before/after reduction, compact/cancel actions.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Failure or insufficient reduction keeps the prior valid basis; pending approvals and unknown external effects cannot be erased by summarization.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W05, W06. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.103; plates pp.127–128; controls `K-01 K-02 K-03 K-04 C-12 I-11 V-07`."
  },
  {
    "id": "S11",
    "title": "Rules, instructions and effective prompt",
    "purpose": "Inspect instructions by origin and activation scope, edit allowed sources and preview the merged bundle the next task will capture; a policy lock is not overridable by text.",
    "action": "Test a scoped rule against sample files, review conflicts and save a version; show a policy lock rather than pretending text can override it.",
    "recovery": "An invalid glob or unresolved template variable blocks Apply; unsaved edits survive navigation and are never mistaken for applied runtime state.",
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
    "layoutRegions": "Source tree, instruction editor, activation/match preview, merged effective prompt, conflict/lock inspector.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: An invalid glob or unresolved template variable blocks Apply; unsaved edits survive navigation and are never mistaken for applied runtime state.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.103; plates pp.129–130; controls `I-01 I-02 I-03 I-04 I-05 I-06 I-07 I-08 I-09 I-10 I-11`."
  },
  {
    "id": "S12",
    "title": "Memory, learned facts and retention",
    "purpose": "Separate learned failure priors, user instructions and proposed personal/project memory; each record has scope, source, date and deletion semantics.",
    "action": "Review a proposed memory diff before saving; open the originating evidence or delete the exact record after scope review.",
    "recovery": "Deleting a memory affects future use after acknowledgement, not copies in prior calls; a project cannot redirect global memory storage.",
    "controls": [
      "K-05",
      "K-10",
      "K-11",
      "O-09",
      "I-11"
    ],
    "layoutRegions": "Scope selector, memory records, provenance/evidence, retention/delete detail, learned-failure section.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Deleting a memory affects future use after acknowledgement, not copies in prior calls; a project cannot redirect global memory storage.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.103; plates pp.131–132; controls `K-05 K-10 K-11 O-09 I-11`."
  },
  {
    "id": "S13",
    "title": "Permissions and sandbox policy editor",
    "purpose": "Explain effective permissions by tool, action, target and scope; show persistent rules separately from pending approvals and actual containment.",
    "action": "Preview a rule match against a sample operation, then apply a versioned policy to future dispatch after daemon acknowledgement.",
    "recovery": "No policy store means no Remember rule; revocation reports future denial and already-issued effects separately; sandbox failure remains visible.",
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
    "layoutRegions": "Permission preset, filesystem/shell/network sections, matcher/rule editor, effective-policy preview, audit/revocation.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: No policy store means no Remember rule; revocation reports future denial and already-issued effects separately; sandbox failure remains visible.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W08, W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.104; plates pp.133–134; controls `Q-01 Q-02 Q-03 Q-04 Q-05 Q-07 Q-08 Q-09 Q-10 Q-11 Q-12 Q-13 Q-14`."
  },
  {
    "id": "S14",
    "title": "Approval: exact operation and safe response",
    "purpose": "Approval dock shows requesting session/child, operation, target, reason and effect boundary without losing the transcript; Allow once/Deny address one exact waiter generation.",
    "action": "Allow once or Deny to one exact waiter generation; Remember opens a separate persistent-policy draft only when supported.",
    "recovery": "Expired, already-replied or wrong-session requests disable response and offer Refresh; a timeout preserves unknown state until reconciled.",
    "controls": [
      "Q-06",
      "Q-07",
      "Q-09",
      "Q-13",
      "X-12",
      "B-12",
      "D-12"
    ],
    "layoutRegions": "Requesting owner/child/generation, exact operation/arguments/cwd/target, effect boundary, Allow once/Deny, Remember draft, expiry.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Expired, already-replied or wrong-session requests disable response and offer Refresh; a timeout preserves unknown state until reconciled.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.104; plates pp.135–136; controls `Q-06 Q-07 Q-09 Q-13 X-12 B-12 D-12`."
  },
  {
    "id": "S15",
    "title": "Tools and MCP connection health",
    "purpose": "Group built-in tools and MCP servers by origin, availability and active permissions; discovered, enabled, healthy and authorized are independently visible.",
    "action": "Open a server to inspect tool schemas, permission defaults, output bounds and logs; reconnect only through owned lifecycle control.",
    "recovery": "Authentication required, failed discovery and stopped process are different states; unknown remote effects never become Cancelled merely on disconnect.",
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
    "layoutRegions": "Built-in tools, MCP server list, tool catalog, permission/default badges, health/auth/logs, reconnect/remove.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Authentication required, failed discovery and stopped process are different states; unknown remote effects never become Cancelled merely on disconnect.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W08. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.104; plates pp.137–138; controls `T-01 T-02 T-03 T-04 T-06 T-07 T-08 T-09 T-10 T-11 T-12 T-13 T-15`."
  },
  {
    "id": "S16",
    "title": "Add MCP server with trust and bounded config",
    "purpose": "Configure a supervised stdio server with exact command/args and explicit trust; remote transport is offered only after capability negotiation; repository content never auto-starts a server.",
    "action": "Validate size/count bounds, review executable host and source, then save and explicitly start the trusted server.",
    "recovery": "A repository-supplied executable never starts merely because a config file exists; invalid args or a failed start preserve the previous valid server.",
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
    "layoutRegions": "Transport selector, identity/command/args, environment references, trust/source review, bounds, test/start actions.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A repository-supplied executable never starts merely because a config file exists; invalid args or a failed start preserve the previous valid server.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W08. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.104; plates pp.139–140; controls `T-03 T-04 T-05 T-06 T-10 T-13 T-14 T-15`."
  },
  {
    "id": "S17",
    "title": "Skills and command discovery",
    "purpose": "Discover reusable workflows without confusing installation with permission grants; commands say whether they insert text, start a task or execute a bounded action.",
    "action": "Open a skill manifest and version diff before enabling; insert a command into a draft without unexpectedly sending it.",
    "recovery": "Unavailable dependencies or a failed update keep the previous known-good version; disabling a skill removes both discovery and agent invocation.",
    "controls": [
      "S-01",
      "S-02",
      "S-03",
      "S-04",
      "S-05",
      "S-06",
      "S-14",
      "I-06"
    ],
    "layoutRegions": "Skill library, manifest/version, source/trust, invocation policy, arguments/template preview, install/update/remove.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unavailable dependencies or a failed update keep the previous known-good version; disabling a skill removes both discovery and agent invocation.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W09. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.105; plates pp.141–142; controls `S-01 S-02 S-03 S-04 S-05 S-06 S-14 I-06`."
  },
  {
    "id": "S18",
    "title": "Hooks: lifecycle automation and testing",
    "purpose": "Edit exact native lifecycle events, executable arguments, environment, bounds and failure policy; test under the same capability envelope; partial output cannot authorize after timeout.",
    "action": "Select an event, inspect matching inputs and run an explicitly admitted test; save only a validated version for future events.",
    "recovery": "Deadline, nonzero exit and malformed verdict remain distinct; partial output cannot become an Allow verdict after timeout.",
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
    "layoutRegions": "Native event list, hook spec form (executable/args/env/deadline/caps/failure), match preview, bounded test result, audit artifact.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Deadline, nonzero exit and malformed verdict remain distinct; partial output cannot become an Allow verdict after timeout.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W10. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.105; plates pp.143–144; controls `S-07 S-08 S-09 S-10 S-11 S-12 S-13 S-14`."
  },
  {
    "id": "S19",
    "title": "Terminal ownership and background work",
    "purpose": "An owned process table and bounded output coexist with the transcript; terminal kill, run stop and session abort are separate actions; stale PIDs cannot authorize kill.",
    "action": "Select a live process, inspect cwd/sandbox/output, then use only controls supported by its current owned handle.",
    "recovery": "A stale PID cannot authorize kill; timeout or disconnect retains Unknown effect until reconciliation, with no blind replay.",
    "controls": [
      "X-01",
      "X-02",
      "X-03",
      "X-04",
      "X-05",
      "X-12",
      "D-07",
      "B-10"
    ],
    "layoutRegions": "Owned process table, cwd/sandbox profile, bounded output, process/task/session controls, artifact links.",
    "vscodeVariant": "Faktor editor tab for wide detail; terminal/output remains bounded and anchored.",
    "jetbrainsVariant": "Tool-window detail with selector/one visible surface; native editor/output tabs for long artifacts.",
    "visual": "12/16 px panel inset; 32 px control rows; bounded output with artifact link; 240 px stacks controls, never hides Stop/attention.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A stale PID cannot authorize kill; timeout or disconnect retains Unknown effect until reconciliation, with no blind replay.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W12. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.105; plates pp.145–146; controls `X-01 X-02 X-03 X-04 X-05 X-12 D-07 B-10`."
  },
  {
    "id": "S20",
    "title": "Browser, diagnostics and editor actions",
    "purpose": "Integrate host diagnostics and explicit code actions; browser controls require an actual advertised adapter with account, machine and network scope.",
    "action": "Capture versioned selection or diagnostics into a draft; connect a browser only through an implemented adapter and scoped permission decision.",
    "recovery": "A screenshot or clean console does not prove functional correctness; unavailable browser support is labelled rather than shown as an idle tab.",
    "controls": [
      "X-06",
      "X-07",
      "X-08",
      "X-09",
      "X-10",
      "X-11",
      "X-13",
      "C-03"
    ],
    "layoutRegions": "Browser adapter/connection, URL/account/machine scope, screenshot/console/network observations, diagnostics and editor actions.",
    "vscodeVariant": "Faktor editor tab for wide detail; terminal/output remains bounded and anchored.",
    "jetbrainsVariant": "Tool-window detail with selector/one visible surface; native editor/output tabs for long artifacts.",
    "visual": "12/16 px panel inset; 32 px control rows; bounded output with artifact link; 240 px stacks controls, never hides Stop/attention.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A screenshot or clean console does not prove functional correctness; unavailable browser support is labelled rather than shown as an idle tab.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.105; plates pp.147–148; controls `X-06 X-07 X-08 X-09 X-10 X-11 X-13 C-03`."
  },
  {
    "id": "S21",
    "title": "Budgets, retry policy and loop guards",
    "purpose": "Show exact run limits and current accounting beside reliability controls; a soft warning is different from enforced admission refusal; a cap decrease cannot erase obligations.",
    "action": "Stage a new default or request a supported active-task allowance revision; clearly label which captured task is affected.",
    "recovery": "Unknown price is not zero; a cap decrease cannot erase settled or reserved spend; retry only after classifying admission and external-effect state.",
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
    "layoutRegions": "Token cap, money cap, threshold/warning, retry policy, loop guards, current ledger/reservation summary.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Unknown price is not zero; a cap decrease cannot erase settled or reserved spend; retry only after classifying admission and external-effect state.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W13. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.106; plates pp.149–150; controls `B-01 B-02 B-03 B-04 B-05 B-06 B-07 B-08 B-09 B-10 B-11 B-12`."
  },
  {
    "id": "S22",
    "title": "Live run: tools, queue and attention",
    "purpose": "A live timeline explains what is active, waiting, queued, failed or uncertain; critical attention remains visible when details are collapsed.",
    "action": "Inspect a tool call, edit the owned next-message queue or stop the current run; a steer action states its delivery point.",
    "recovery": "Lost stream marks the projection stale without turning the task idle; reconnect from durable cursor and reconcile before enabling duplicate effects.",
    "controls": [
      "D-05",
      "D-07",
      "D-08",
      "D-12",
      "D-13",
      "M-11",
      "G-14",
      "Q-06",
      "T-09",
      "B-04"
    ],
    "layoutRegions": "Run header, live timeline, tool-call detail, prompt queue, attention/approval dock, composer/status.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Lost stream marks the projection stale without turning the task idle; reconnect from durable cursor and reconcile before enabling duplicate effects.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W07, W10, W12, W13. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.106; plates pp.151–152; controls `D-05 D-07 D-08 D-12 D-13 M-11 G-14 Q-06 T-09 B-04`."
  },
  {
    "id": "S23",
    "title": "Agents: delegation, ownership and handoff",
    "purpose": "Inspect parent and child work as a tree with bounded concurrency, shared spend and candidate ownership; child output is evidence, not automatic parent success.",
    "action": "Select a child to view its admitted role/model/tools and use supported Steer, Model or Stop controls.",
    "recovery": "Stale child controls are fenced by session and generation; child failure preserves artifacts and the parent remaining obligations.",
    "controls": [
      "A-06",
      "A-07",
      "A-08",
      "A-09",
      "A-10",
      "A-11",
      "A-12",
      "M-11",
      "B-04",
      "Q-06"
    ],
    "layoutRegions": "Agent tree, child role/model/status, parent budget/ownership, child controls, typed handoff/result.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Stale child controls are fenced by session and generation; child failure preserves artifacts and the parent remaining obligations.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W11. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.106; plates pp.153–154; controls `A-06 A-07 A-08 A-09 A-10 A-11 A-12 M-11 B-04 Q-06`."
  },
  {
    "id": "S24",
    "title": "Native changes and review obligations",
    "purpose": "Use each host native diff editor; keep base/candidate identity, acceptance criteria and remaining verification adjacent to code review; review marks do not integrate or publish.",
    "action": "Open a file/hunk, attach a versioned comment or prepare a new bounded task; review marks alone do not integrate or publish.",
    "recovery": "Manual owner edits trigger compare-and-swap refusal; a changed or partially selected candidate needs matching new evidence.",
    "controls": [
      "V-01",
      "V-02",
      "V-03",
      "V-07",
      "V-09",
      "V-11",
      "V-12",
      "A-11"
    ],
    "layoutRegions": "File tree, native diff, versioned comments, acceptance criteria, verification/proof status, review/finish actions.",
    "vscodeVariant": "Native VS Code diff editor/tab; review marks never land a change.",
    "jetbrainsVariant": "Native IntelliJ diff editor; review marks never land a change.",
    "visual": "Native diff editor chrome; review/details inset 12–24 px; version badges and 32 px actions; no overlap of verification status with code.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Manual owner edits trigger compare-and-swap refusal; a changed or partially selected candidate needs matching new evidence.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W14, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.106; plates pp.155–156; controls `V-01 V-02 V-03 V-07 V-09 V-11 V-12 A-11`."
  },
  {
    "id": "S25",
    "title": "Checkpoints, restore and fork preview",
    "purpose": "Show recorded checkpoint identity, content coverage and branching separately; preview exact files and conversation range before any supported restore.",
    "action": "Choose a stored checkpoint, inspect coverage and compare content preconditions before requesting a separate restore transaction.",
    "recovery": "Hash-only entries may not support redo/diff; unrelated edits or missing content refuse restore; shell/network/SCM effects are disclosed as uncovered.",
    "controls": [
      "V-04",
      "V-05",
      "V-06",
      "V-12",
      "D-06",
      "Q-09"
    ],
    "layoutRegions": "Checkpoint timeline, coverage/preconditions, diff preview, restore/fork scope, uncovered-effects warning.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Hash-only entries may not support redo/diff; unrelated edits or missing content refuse restore; shell/network/SCM effects are disclosed as uncovered.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W14. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.107; plates pp.157–158; controls `V-04 V-05 V-06 V-12 D-06 Q-09`."
  },
  {
    "id": "S26",
    "title": "History, search and parallel workspaces",
    "purpose": "History shows durable conversation identity, worktree, status and engineering outcome; label title search separately from transcript find and indexed recall.",
    "action": "Switch deliberately while retaining per-session drafts, scroll and pending attention; open supported session actions with precise retention scope.",
    "recovery": "Late callbacks cannot populate the wrong conversation; an unavailable history service is not an empty list; archived is not deleted.",
    "controls": [
      "D-01",
      "D-02",
      "D-03",
      "D-04",
      "D-06",
      "D-13",
      "D-15",
      "V-09"
    ],
    "layoutRegions": "Filters (title, status, worktree, outcome), paged session list, selected session summary, actions, parallel worktree tabs.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Late callbacks cannot populate the wrong conversation; an unavailable history service is not an empty list; archived is not deleted.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W12, W13, W14, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.107; plates pp.159–160; controls `D-01 D-02 D-03 D-04 D-06 D-13 D-15 V-09`."
  },
  {
    "id": "S27",
    "title": "Usage, context and engineering outcomes",
    "purpose": "Separate token accounting, provider/cache prices, quota, local estimates and verification-based engineering cost; explain every denominator.",
    "action": "Filter by workspace, model, cohort and time basis, then inspect the underlying attempted runs and verified landed changes.",
    "recovery": "No verified changes means VCSC unavailable, not zero; cancelled/failed attempts remain in the eligible spend numerator.",
    "controls": [
      "B-04",
      "M-13",
      "K-12",
      "O-06",
      "V-09",
      "D-07"
    ],
    "layoutRegions": "Period/cohort filters, token/cost categories, provider/cache/quota, engineering outcomes, VCSC and denominators.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: No verified changes means VCSC unavailable, not zero; cancelled/failed attempts remain in the eligible spend numerator.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W13, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.107; plates pp.161–162; controls `B-04 M-13 K-12 O-06 V-09 D-07`."
  },
  {
    "id": "S28",
    "title": "Routing, efficiency and retrieval controls",
    "purpose": "Expose current production efficiency flags and off-switches with honest measurement, fallback and apply timing; duplicate semantic_context/information_gain aliases fail validation.",
    "action": "Preview a config diff, validate aliases and save through supported operator/native pathways; keep provenance for each effective value.",
    "recovery": "Missing embedding support falls back to lexical/symbol retrieval; duplicate semantic_context/information_gain aliases fail validation.",
    "controls": [
      "M-04",
      "M-06",
      "M-10",
      "K-05",
      "K-06",
      "K-07",
      "K-08",
      "K-09",
      "K-12",
      "C-09",
      "C-10",
      "C-11"
    ],
    "layoutRegions": "Routing policy, efficiency flags (failure learning, CCR, typed handoff, semantic context, rework), retrieval strategy, measured effects.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Missing embedding support falls back to lexical/symbol retrieval; duplicate semantic_context/information_gain aliases fail validation.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.107; plates pp.163–164; controls `M-04 M-06 M-10 K-05 K-06 K-07 K-08 K-09 K-12 C-09 C-10 C-11`."
  },
  {
    "id": "S29",
    "title": "Settings: scope, provenance and atomic apply",
    "purpose": "Search settings across allowed scopes and inspect each effective value, source, lock and timing; distinguish staged, saved and runtime-applied state; apply one validated atomic patch.",
    "action": "Apply a validated atomic patch against a known config revision, or Reset to applied values; Restore defaults edits the form until Apply.",
    "recovery": "A revision conflict opens a compare/rebase view; machine executable paths never inherit from untrusted project settings.",
    "controls": [
      "O-01",
      "O-04",
      "O-07",
      "O-08",
      "O-10",
      "O-12",
      "O-13",
      "D-09",
      "D-10",
      "D-11",
      "D-14"
    ],
    "layoutRegions": "Scope/search, category navigation, form rows with effective source/lock, staged changes, Reset/Apply/Restore defaults.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A revision conflict opens a compare/rebase view; machine executable paths never inherit from untrusted project settings.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: no dedicated W01–W16 path; reached from Settings/editor. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.108; plates pp.165–166; controls `O-01 O-04 O-07 O-08 O-10 O-12 O-13 D-09 D-10 D-11 D-14`."
  },
  {
    "id": "S30",
    "title": "Diagnostics, runtime trust and recovery",
    "purpose": "Show readiness, version compatibility, runtime identity and bounded diagnostics with an actionable typed cause; checksum mismatch refuses start; checksum match is not signature verification.",
    "action": "Review a redacted diagnostic preview before export; explicit Start is separate from opening the tool window.",
    "recovery": "Checksum mismatch refuses start; an unrecorded exact path needs deliberate trust; a checksum match does not claim publisher-signature verification.",
    "controls": [
      "O-03",
      "O-04",
      "O-05",
      "O-07",
      "O-09",
      "O-13",
      "B-11",
      "B-12",
      "D-13"
    ],
    "layoutRegions": "Readiness/version, runtime path/checksum/trust, protocol compatibility, bounded diagnostics, logs, start/export/recovery actions.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Checksum mismatch refuses start; an unrecorded exact path needs deliberate trust; a checksum match does not claim publisher-signature verification.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W10, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.108; plates pp.167–168; controls `O-03 O-04 O-05 O-07 O-09 O-13 B-11 B-12 D-13`."
  },
  {
    "id": "S31",
    "title": "Remote workspaces and execution boundary",
    "purpose": "Display editor, execution, model, vault and candidate locations together; remote workspaces must not blur local/hosted boundaries or path ownership.",
    "action": "Bind a conversation to one workspace/worktree/daemon generation; revalidate after container, SSH or execution-host changes.",
    "recovery": "A host change invalidates process and permission handles until rebound; preserve drafts without replaying a captured effectful request elsewhere.",
    "controls": [
      "X-13",
      "P-13",
      "O-02",
      "O-07",
      "O-11",
      "D-01",
      "D-15",
      "A-11"
    ],
    "layoutRegions": "Binding identity (workspace/worktree/daemon), editor/execution/model/vault/worker locations, data boundary, revalidation/rebind.",
    "vscodeVariant": "Editor tab `Faktor: <Category>` in the Faktor Settings group; per-field effective source; staged changes footer; `Reset`/`Apply changes`.",
    "jetbrainsVariant": "`Settings › Tools › Faktor`, category tree, one shared application-scoped validation/persistence controller; `Cancel`/`Apply`/`OK`; project scope is proposed.",
    "visual": "Two-column form at ≥480 px (label 10–15rem, flexible value); 12 px row gap; 24 px groups; 32 px controls; pinned staged footer; 240/320 px stack with no clipped labels.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: A host change invalidates process and permission handles until rebound; preserve drafts without replaying a captured effectful request elsewhere.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W02, W16. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.108; plates pp.169–170; controls `X-13 P-13 O-02 O-07 O-11 D-01 D-15 A-11`."
  },
  {
    "id": "S32",
    "title": "Task outcome and finish proof",
    "purpose": "Finish with explicit engineering status: required acceptance, claim evidence, matching integration and requested finish; conversation completion alone proves none of them.",
    "action": "Open each evidence receipt, inspect matching candidate identity and resolve any missing required fact before the requested finish can be fulfilled.",
    "recovery": "Missing unit evidence, failed integration or absent SCM configuration remains a named blocker; a successful chat response cannot turn it green.",
    "controls": [
      "V-07",
      "V-08",
      "V-09",
      "V-10",
      "V-11",
      "V-12",
      "O-06",
      "B-04"
    ],
    "layoutRegions": "Engineering outcome, required criteria, claim evidence, candidate/proof identity, landing/integration, finish level/receipt.",
    "vscodeVariant": "Faktor activity view; wide diff/context/call detail opens in a native editor tab; Back restores the prior selection.",
    "jetbrainsVariant": "Faktor tool window; wide detail opens in the native editor/diff; tool-window selection and text caret survive snapshots.",
    "visual": "12/16 px panel inset; 8 px control gap; 4/8 px inline gaps; 32 px composer; attention rail remains visible at 240 px.",
    "states": "Empty: name the add/discover/refresh action and show no placeholder values. Loading: bounded/cancellable, keep the last coherent state. Stale: show age and disable admission/apply until reconciled. Error: typed cause, affected scope and safe recovery; preserve the draft. Disabled: show the missing capability/policy/ownership prerequisite; never render an inert toggle (p.013). Screen-specific recovery: Missing unit evidence, failed integration or absent SCM configuration remains a named blocker; a successful chat response cannot turn it green.",
    "navigation": "Reached from the Faktor view/settings entry and the native command/keymap; flows: W06, W15. Back or Escape restores the invoking selection; session/worktree switches preserve per-session drafts and never merge late callbacks (pp.007–008, 013).",
    "traceability": "Contract p.108; plates pp.171–172; controls `V-07 V-08 V-09 V-10 V-11 V-12 O-06 B-04`."
  }
];

export const E5_CATEGORIES: readonly Edition5Category[] = [
  {
    "code": "M",
    "name": "Models and routing",
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
    ]
  },
  {
    "code": "P",
    "name": "Providers, credentials and local inference",
    "controls": [
      "P-01",
      "P-02",
      "P-03",
      "P-04",
      "P-05",
      "P-06",
      "P-07",
      "P-08",
      "P-09",
      "P-10",
      "P-11",
      "P-12",
      "P-13",
      "P-14",
      "P-15"
    ]
  },
  {
    "code": "G",
    "name": "Reasoning and generation",
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
    ]
  },
  {
    "code": "A",
    "name": "Agent presets and delegation",
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
    ]
  },
  {
    "code": "C",
    "name": "Context, attachments and retrieval",
    "controls": [
      "C-01",
      "C-02",
      "C-03",
      "C-04",
      "C-05",
      "C-06",
      "C-07",
      "C-08",
      "C-09",
      "C-10",
      "C-11",
      "C-12",
      "C-13",
      "C-14"
    ]
  },
  {
    "code": "K",
    "name": "Compaction, memory and efficiency",
    "controls": [
      "K-01",
      "K-02",
      "K-03",
      "K-04",
      "K-05",
      "K-06",
      "K-07",
      "K-08",
      "K-09",
      "K-10",
      "K-11",
      "K-12"
    ]
  },
  {
    "code": "I",
    "name": "Instructions, rules and prompt composition",
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
    ]
  },
  {
    "code": "Q",
    "name": "Permissions, autonomy and sandbox",
    "controls": [
      "Q-01",
      "Q-02",
      "Q-03",
      "Q-04",
      "Q-05",
      "Q-06",
      "Q-07",
      "Q-08",
      "Q-09",
      "Q-10",
      "Q-11",
      "Q-12",
      "Q-13",
      "Q-14"
    ]
  },
  {
    "code": "T",
    "name": "Tool catalog and MCP",
    "controls": [
      "T-01",
      "T-02",
      "T-03",
      "T-04",
      "T-05",
      "T-06",
      "T-07",
      "T-08",
      "T-09",
      "T-10",
      "T-11",
      "T-12",
      "T-13",
      "T-14",
      "T-15"
    ]
  },
  {
    "code": "S",
    "name": "Skills, commands and hooks",
    "controls": [
      "S-01",
      "S-02",
      "S-03",
      "S-04",
      "S-05",
      "S-06",
      "S-07",
      "S-08",
      "S-09",
      "S-10",
      "S-11",
      "S-12",
      "S-13",
      "S-14"
    ]
  },
  {
    "code": "X",
    "name": "Terminal, browser and language tooling",
    "controls": [
      "X-01",
      "X-02",
      "X-03",
      "X-04",
      "X-05",
      "X-06",
      "X-07",
      "X-08",
      "X-09",
      "X-10",
      "X-11",
      "X-12",
      "X-13"
    ]
  },
  {
    "code": "B",
    "name": "Budgets, retries and reliability",
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
    ]
  },
  {
    "code": "V",
    "name": "Changes, checkpoints, verification and finish",
    "controls": [
      "V-01",
      "V-02",
      "V-03",
      "V-04",
      "V-05",
      "V-06",
      "V-07",
      "V-08",
      "V-09",
      "V-10",
      "V-11",
      "V-12"
    ]
  },
  {
    "code": "D",
    "name": "Sessions, displays and accessibility",
    "controls": [
      "D-01",
      "D-02",
      "D-03",
      "D-04",
      "D-05",
      "D-06",
      "D-07",
      "D-08",
      "D-09",
      "D-10",
      "D-11",
      "D-12",
      "D-13",
      "D-14",
      "D-15"
    ]
  },
  {
    "code": "O",
    "name": "Privacy, diagnostics, optional cloud and distribution",
    "controls": [
      "O-01",
      "O-02",
      "O-03",
      "O-04",
      "O-05",
      "O-06",
      "O-07",
      "O-08",
      "O-09",
      "O-10",
      "O-11",
      "O-12",
      "O-13"
    ]
  }
];

export const E5_CONTROL_BY_ID: ReadonlyMap<string, Edition5Control> = new Map(
  E5_CONTROLS.map((control) => [control.id, control]),
);

export const E5_SCREEN_BY_ID: ReadonlyMap<string, Edition5Screen> = new Map(
  E5_SCREENS.map((screen) => [screen.id, screen]),
);
