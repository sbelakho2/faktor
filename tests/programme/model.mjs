#!/usr/bin/env node
// Core model for the automated test programme (Report.pdf, Oct 18 2026).
//
// The programme is a per-surface requirement registry, not a second list of
// generic invariants. Every discovered product surface gets its own row with
// its own oracle/test ids, owning layer(s), lane(s), evidence kind and derived
// status. The six layers and five lanes below are the report's execution
// model; a release needs every lane green at one commit.

export const REGISTRY_SCHEMA = 'faktor-test-programme-registry/v1';
export const STATUS_SCHEMA = 'faktor-test-programme-status/v1';
export const LANE_SCHEMA = 'faktor-programme-lane/v1';
export const SELECTION_SCHEMA = 'faktor-change-selection/v1';
export const RELEASE_SCHEMA = 'faktor-programme-release-gate/v1';

// Six layers from the report's page-2 diagram (not a structural layer:
// static/registry checks are gates, not one of the six proof layers).
export const LAYERS = Object.freeze({
  'unit-property': {
    id: 'unit-property',
    description:
      'Unit and property: crate tests, generated inputs, shrinking/fixed seeds, reference oracles and invariant mutations.',
  },
  'state-model-crash': {
    id: 'state-model-crash',
    description:
      'State model and crash: journal/ledger/CAS/CAS-rename state models, kill-at-write campaigns, deterministic interleavings and store faults.',
  },
  contracts: {
    id: 'contracts',
    description:
      'Contracts: frozen DTOs/wire/CLI/route contracts, fake vs live parity, transport chaos, vendor/MCP/SCM canaries and static authority.',
  },
  'ide-screen-readers': {
    id: 'ide-screen-readers',
    description:
      'IDE and screen readers: real VS Code/JetBrains hosts, controls/screens/workflow/acceptance cells, accessibility trees and visual baselines.',
  },
  'agent-evals': {
    id: 'agent-evals',
    description:
      'Agent evals: hidden-task corpus, nightly smoke, real-model behaviour and quality/cost trade-offs (never a lower-layer excuse).',
  },
  hardware: {
    id: 'hardware',
    description:
      'Hardware: Renode/QEMU corpus and the HIL rig; a board result is never inherited from a simulator or another platform.',
  },
});

// Five lanes (when/where a proof runs), straight from the report.
export const LANES = Object.freeze({
  pull: {
    id: 'pull',
    description:
      'Every pull request, Linux, no secrets. Fast affected suites by default; merge gate.',
    layers: ['unit-property', 'state-model-crash', 'contracts'],
    full: false,
  },
  trusted: {
    id: 'trusted',
    description:
      'Push to main on Linux/macOS/Windows. Full workspace, fault campaigns, contracts, real-IDEs and install matrix; release candidates.',
    layers: ['unit-property', 'state-model-crash', 'contracts', 'ide-screen-readers'],
    full: true,
  },
  nightly: {
    id: 'nightly',
    description:
      'Schedule. Fault at scale, fuzzing, mutation sampling, interleaving, scale probe, churn, resource census, soak, eval smoke and UI smoke.',
    layers: ['unit-property', 'state-model-crash', 'contracts', 'ide-screen-readers', 'agent-evals'],
    full: true,
  },
  'weekly-live': {
    id: 'weekly-live',
    description:
      'Schedule on operator keys/accounts. Live provider/vendor/MCP/SCM contracts, full eval, screen readers and Renode.',
    layers: ['contracts', 'ide-screen-readers', 'agent-evals'],
    full: true,
  },
  release: {
    id: 'release',
    description:
      'Tag. All six layers at one commit plus hardware-in-the-loop and signed evidence.',
    layers: ['unit-property', 'state-model-crash', 'contracts', 'ide-screen-readers', 'agent-evals', 'hardware'],
    full: true,
  },
});

export const STATUSES = Object.freeze(['proven', 'deferred', 'stub', 'failed']);

export const ADVERSARIAL_BY_KIND = Object.freeze({
  'native-route': ['auth', 'strict-body', 'typed-error', 'bounds', 'traversal', 'duplicate', 'isolation', 'no-secret'],
  'cli-command': ['help', 'unknown-flag', 'missing-daemon', 'redaction', 'idempotence'],
  'vscode-contribution': ['registered', 'arg-schema', 'disabled-state', 'host-parity'],
  'vscode-setting': ['default', 'invalid-value', 'applies-at'],
  'jetbrains-contribution': ['registered', 'palette-reach', 'disabled-state', 'host-parity'],
  'builtin-tool': ['denied', 'timeout', 'cancel', 'output-cap', 'path-escape', 'bad-args', 'child-cleanup'],
  'provider-adapter': ['fake-live-parity', 'error-mapping', 'retry', 'locality', 'capability-selection'],
  control: ['default-scope-timing', 'empty-loading-error', 'stale', 'keyboard', 'screen-reader'],
  screen: ['empty', 'loading', 'error', 'narrow', 'theme', 'focus-order'],
  workflow: ['pointer', 'keyboard', 'screen-reader', 'compact', 'crash-mid-flow', 'network-loss'],
  'acceptance-scenario': ['documented-case', 'forbidden-state', 'host-parity'],
  invariant: ['planted-mutation', 'wiring'],
  'config-key': ['default', 'invalid', 'provenance', 'unknown-key'],
  adapter: ['recorded-drift', 'hostile-response', 'live-canary'],
  'act-now': ['hostile-input', 'replay', 'corruption'],
});

export const EVIDENCE_KINDS = Object.freeze([
  'exact-value',
  'reference-implementation',
  'metamorphic',
  'property',
  'state-model',
  'recorded-real-world',
  'measurement',
  'static-scan',
  'live-observation',
]);

// A row is delegation-only when every test id it names is a shared invariant
// rather than a behaviour test of the surface itself. The report's rule 8.
export function isDelegationOnly(testIds) {
  if (!Array.isArray(testIds) || testIds.length === 0) return true;
  return testIds.every((id) => {
    const value = String(id);
    return (
      /^INV-[A-Z0-9-]+$/.test(value) ||
      value === 'INV-CLIENT-ROUTE-CONTRACT' ||
      value === 'INV-PROVIDER-CLASSIFICATION-TRUTH-TABLE' ||
      value === 'INV-AUTH-REQUIRED' ||
      value === 'INV-ROUTE-CONTRACT'
    );
  });
}

export function layerIds() {
  return Object.keys(LAYERS);
}

export function laneIds() {
  return Object.keys(LANES);
}

export function layerLanes(layer) {
  return laneIds().filter((lane) => LANES[lane].layers.includes(layer));
}

export function rowProblems(row) {
  const problems = [];
  const where = row?.id ?? '<row>';
  if (typeof row?.id !== 'string' || row.id.length === 0) {
    return [`row: missing id`];
  }
  if (typeof row.kind !== 'string' || row.kind.length === 0) {
    problems.push(`${where}: missing kind`);
  }
  if (typeof row.path !== 'string' || row.path.length === 0) {
    problems.push(`${where}: missing path`);
  }
  if (!Array.isArray(row.layers) || row.layers.length === 0) {
    problems.push(`${where}: no owning layer`);
  } else {
    for (const layer of row.layers) {
      if (!LAYERS[layer]) problems.push(`${where}: unknown layer ${layer}`);
    }
  }
  if (!Array.isArray(row.lanes) || row.lanes.length === 0) {
    problems.push(`${where}: no lane`);
  } else {
    for (const lane of row.lanes) {
      if (!LANES[lane]) problems.push(`${where}: unknown lane ${lane}`);
    }
  }
  if (!Array.isArray(row.test_ids) || row.test_ids.length === 0) {
    problems.push(`${where}: no test id`);
  } else if (isDelegationOnly(row.test_ids)) {
    problems.push(`${where}: delegation-only row (no own behaviour test)`);
  }
  if (!EVIDENCE_KINDS.includes(row.evidence_kind)) {
    problems.push(`${where}: unknown evidence kind ${JSON.stringify(row.evidence_kind)}`);
  }
  if (typeof row.oracle !== 'string' || row.oracle.trim().length < 12) {
    problems.push(`${where}: oracle must state a falsifiable expected result`);
  }
  if (!Array.isArray(row.adversarial) || row.adversarial.length === 0) {
    problems.push(`${where}: no adversarial cases`);
  }
  if (row.delegate !== undefined) {
    problems.push(`${where}: delegation is forbidden; rows carry own tests`);
  }
  if (!STATUSES.includes(row.status)) {
    problems.push(`${where}: unknown status ${JSON.stringify(row.status)}`);
  }
  if (row.status === 'proven' && row.evidence?.result?.passed !== true && row.evidence?.passed !== true) {
    problems.push(`${where}: proven row has no passing evidence in this run`);
  }
  if (Array.isArray(row.layers) && Array.isArray(row.lanes)) {
    for (const layer of row.layers) {
      if (!row.lanes.some((lane) => LANES[lane]?.layers.includes(layer))) {
        problems.push(`${where}: layer ${layer} is not run by any assigned lane`);
      }
    }
  }
  return problems;
}

// Deterministic JSON with sorted keys for digests/evidence.
export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value === undefined ? null : value);
}

export function sortRows(rows) {
  return [...rows].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

export function summarizeRegistry(registry) {
  const rows = registry.rows ?? [];
  const byKind = {};
  const byStatus = {};
  const byLane = {};
  const byLayer = {};
  for (const row of rows) {
    byKind[row.kind] = (byKind[row.kind] ?? 0) + 1;
    byStatus[row.status] = (byStatus[row.status] ?? 0) + 1;
    for (const lane of row.lanes ?? []) byLane[lane] = (byLane[lane] ?? 0) + 1;
    for (const layer of row.layers ?? []) byLayer[layer] = (byLayer[layer] ?? 0) + 1;
  }
  return {
    rows: rows.length,
    by_kind: Object.fromEntries(Object.entries(byKind).sort()),
    by_status: Object.fromEntries(Object.entries(byStatus).sort()),
    by_lane: Object.fromEntries(Object.entries(byLane).sort()),
    by_layer: Object.fromEntries(Object.entries(byLayer).sort()),
  };
}
