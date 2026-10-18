#!/usr/bin/env node
// Edition 5 registry: the single machine-truth loader behind every edition5
// gate. It deliberately does not own behavior; it validates that the frozen
// artifacts agree with each other and derives the deterministic exercise
// plans both hosts consume.
//
// Inputs (frozen machine truth):
//   docs/specs/edition5/controls.json  200 control contracts
//   docs/specs/edition5/screens.md     32 screens (+ their control sets)
//   docs/specs/edition5/flows.md       16 flows
//   docs/specs/edition5/acceptance.md  40 acceptance scenarios A01-A40
//
// Every derived plan is stable and content-addressed so the VS Code and
// JetBrains harnesses cannot drift from each other or from the registry.

import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const SPEC_DIR = resolve(ROOT, 'docs/specs/edition5');
export const CONTROLS_PATH = resolve(SPEC_DIR, 'controls.json');
export const SCREENS_PATH = resolve(SPEC_DIR, 'screens.md');
export const FLOWS_PATH = resolve(SPEC_DIR, 'flows.md');
export const ACCEPTANCE_PATH = resolve(SPEC_DIR, 'acceptance.md');

export const EXPECTED = Object.freeze({
  controls: 200,
  screens: 32,
  flows: 16,
  acceptance: 40,
  categories: 15,
});

const CATEGORY_SURFACE = {
  M: 'models',
  P: 'providers',
  G: 'generation',
  A: 'agents',
  C: 'context',
  K: 'compaction-memory',
  I: 'instructions',
  Q: 'permissions',
  T: 'tools-mcp',
  S: 'skills-hooks',
  X: 'terminal-browser',
  B: 'budgets',
  V: 'changes-proof',
  D: 'sessions-display',
  O: 'privacy-diagnostics',
};

/** The screen -> surface grouping is the binding map the hosts implement. */
const SCREEN_SURFACE = {
  S01: 'work',
  S02: 'models',
  S03: 'providers',
  S04: 'providers',
  S05: 'generation',
  S06: 'agents',
  S07: 'agents',
  S08: 'context',
  S09: 'context',
  S10: 'compaction-memory',
  S11: 'instructions',
  S12: 'compaction-memory',
  S13: 'permissions',
  S14: 'permissions',
  S15: 'tools-mcp',
  S16: 'tools-mcp',
  S17: 'skills-hooks',
  S18: 'skills-hooks',
  S19: 'terminal-browser',
  S20: 'terminal-browser',
  S21: 'budgets',
  S22: 'work',
  S23: 'agents',
  S24: 'changes-proof',
  S25: 'changes-proof',
  S26: 'sessions-display',
  S27: 'privacy-diagnostics',
  S28: 'privacy-diagnostics',
  S29: 'privacy-diagnostics',
  S30: 'privacy-diagnostics',
  S31: 'sessions-display',
  S32: 'changes-proof',
};

export function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

export function sha256(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

export function loadControls(path = CONTROLS_PATH) {
  return readJson(path);
}

/** Parse `## S01 — ...` and the following `**Controls.**` bullet. */
export function parseScreens(markdown = readFileSync(SCREENS_PATH, 'utf8')) {
  const screens = [];
  const re = /^## (S\d{2}) — (.+)$/gm;
  let match = null;
  while ((match = re.exec(markdown)) !== null) {
    const start = match.index;
    const next = markdown.indexOf('\n## ', start + 1);
    const body = markdown.slice(start, next === -1 ? undefined : next);
    const controlsMatch = /\*\*Controls\.\*\* `([^`]+)`/.exec(body);
    const controls = controlsMatch
      ? controlsMatch[1]
          .trim()
          .split(/\s+/)
          .filter((id) => /^[A-Z]-\d{2}$/.test(id))
      : [];
    screens.push({ id: match[1], title: match[2].trim(), controls });
  }
  return screens;
}

/** Parse `## W01 — ...` headings. */
export function parseFlows(markdown = readFileSync(FLOWS_PATH, 'utf8')) {
  const flows = [];
  const re = /^## (W\d{2}) — (.+)$/gm;
  let match = null;
  while ((match = re.exec(markdown)) !== null) {
    flows.push({ id: match[1], title: match[2].trim() });
  }
  return flows;
}

/** Parse `## A01 — ...` headings and the `**Source:**` page. */
export function parseAcceptance(markdown = readFileSync(ACCEPTANCE_PATH, 'utf8')) {
  const scenarios = [];
  const re = /^## (A\d{2}) — (.+)$/gm;
  let match = null;
  while ((match = re.exec(markdown)) !== null) {
    const start = match.index;
    const next = markdown.indexOf('\n## ', start + 1);
    const body = markdown.slice(start, next === -1 ? undefined : next);
    const source = /\*\*Source:\*\* (.+)/.exec(body);
    const forbidden = /\*\*Forbidden fake states:\*\* (.+)/.exec(body);
    const fields = /\*\*Field-level checks:\*\* (.+)/.exec(body);
    scenarios.push({
      id: match[1],
      title: match[2].trim(),
      source: source ? source[1].trim() : null,
      forbidden_fake_states: forbidden ? forbidden[1].trim() : null,
      field_checks: fields ? fields[1].trim() : null,
    });
  }
  return scenarios;
}

/**
 * Validate the frozen artifacts against each other. Returns a list of
 * problems; an empty list is the only acceptable registry state.
 */
export function validateRegistry(registry = buildRegistry()) {
  const problems = [];
  const { controls, screens, flows, acceptance } = registry;

  if (controls.records.length !== EXPECTED.controls) {
    problems.push(
      `controls.json: expected ${EXPECTED.controls} records, found ${controls.records.length}`,
    );
  }
  const controlIds = controls.records.map((record) => record.id);
  const uniqueControls = new Set(controlIds);
  if (uniqueControls.size !== controlIds.length) {
    problems.push('controls.json: duplicate control ids');
  }
  const categories = new Set(controls.records.map((record) => record.category_code));
  if (categories.size !== EXPECTED.categories) {
    problems.push(
      `controls.json: expected ${EXPECTED.categories} categories, found ${categories.size}`,
    );
  }
  if (screens.length !== EXPECTED.screens) {
    problems.push(`screens.md: expected ${EXPECTED.screens} screens, found ${screens.length}`);
  }
  if (flows.length !== EXPECTED.flows) {
    problems.push(`flows.md: expected ${EXPECTED.flows} flows, found ${flows.length}`);
  }
  if (acceptance.length !== EXPECTED.acceptance) {
    problems.push(
      `acceptance.md: expected ${EXPECTED.acceptance} scenarios, found ${acceptance.length}`,
    );
  }

  const screenIds = new Set(screens.map((screen) => screen.id));
  const acceptanceIds = new Set(acceptance.map((scenario) => scenario.id));
  const citedAcceptance = new Set();
  const screenControlUnion = new Set();

  for (const screen of screens) {
    if (screen.controls.length === 0) {
      problems.push(`screens.md: ${screen.id} declares no controls`);
    }
    for (const id of screen.controls) {
      if (!uniqueControls.has(id)) {
        problems.push(`screens.md: ${screen.id} names unknown control ${id}`);
      }
      screenControlUnion.add(id);
    }
  }

  for (const record of controls.records) {
    for (const host of ['vscode', 'jetbrains']) {
      if (!Array.isArray(record.hosts) || !record.hosts.includes(host)) {
        problems.push(`${record.id}: missing required host ${host}`);
      }
    }
    if (!Array.isArray(record.screens) || record.screens.length === 0) {
      problems.push(`${record.id}: no screens`);
    }
    for (const screen of record.screens ?? []) {
      if (!screenIds.has(screen)) {
        problems.push(`${record.id}: names unknown screen ${screen}`);
      }
    }
    if (!screenControlUnion.has(record.id)) {
      problems.push(`${record.id}: not present in any screen's control set`);
    }
    if (!Array.isArray(record.acceptance_scenario_ids) || record.acceptance_scenario_ids.length === 0) {
      problems.push(`${record.id}: no acceptance scenario mapping`);
    }
    for (const id of record.acceptance_scenario_ids ?? []) {
      if (!acceptanceIds.has(id)) {
        problems.push(`${record.id}: names unknown acceptance scenario ${id}`);
      }
      citedAcceptance.add(id);
    }
    if (
      (record.current_status === 'partial' || record.current_status === 'missing') &&
      (typeof record.status_note !== 'string' || record.status_note.trim() === '')
    ) {
      problems.push(`${record.id}: ${record.current_status} without a machine-truth status_note`);
    }
    if (record.current_status !== 'exists' && record.current_status !== 'partial' && record.current_status !== 'missing') {
      problems.push(`${record.id}: unknown current_status ${String(record.current_status)}`);
    }
    if (!Array.isArray(record.config_keys)) {
      problems.push(`${record.id}: config_keys must be an array`);
    }
    for (const key of record.config_keys ?? []) {
      if (typeof key !== 'string' || key.trim() === '') {
        problems.push(`${record.id}: empty config key`);
      }
    }
    if (record.evidence !== undefined && !Array.isArray(record.evidence)) {
      problems.push(`${record.id}: evidence must be an array when present`);
    }
  }

  for (const scenario of acceptance) {
    if (!citedAcceptance.has(scenario.id)) {
      problems.push(`${scenario.id}: no control cites this acceptance scenario`);
    }
    if (!scenario.forbidden_fake_states) {
      problems.push(`${scenario.id}: forbidden fake states must be stated`);
    }
    if (!scenario.field_checks) {
      problems.push(`${scenario.id}: field-level checks must be stated`);
    }
  }

  if (registry.inputs_digest !== buildRegistry().inputs_digest) {
    // Defensive: buildRegistry is deterministic; this catches a modified file
    // list without a digest refresh in test doubles.
    problems.push('registry: inputs digest is not stable');
  }
  return problems;
}

/** Stable widget kind from the control contract text. */
export function widgetKind(record) {
  const text = `${record.control} ${record.behavior}`.toLowerCase();
  if (/shortcut|keyboard|hotkey/.test(text)) return 'shortcut';
  if (/toggle|switch|checkbox|enable|disable/.test(text)) return 'toggle';
  if (/menu|command palette|context menu|action/.test(text)) return 'menu-item';
  if (/input|field|editor|text box|textbox|textarea|number entry|search/.test(text)) return 'input';
  if (/picker|select|dropdown|combo|list/.test(text)) return 'select';
  if (/button|save|apply|reset|refresh|add|remove|delete|retry|cancel|start|stop|send/.test(text)) {
    return 'button';
  }
  return 'control';
}

/**
 * Derive one host exercise plan. The plan is not a promise that the host has
 * implemented the control: it names the real surface the host harness must
 * drive and, when the control is absent there, the machine-truth reason the
 * harness must render/observe as an explicit unavailable state.
 */
export function buildExercisePlan(host, registry = buildRegistry()) {
  if (host !== 'vscode' && host !== 'jetbrains') {
    throw new Error(`unknown host ${host}`);
  }
  const plan = registry.controls.records.map((record) => {
    const surface =
      SCREEN_SURFACE[record.catalog_screen] ?? CATEGORY_SURFACE[record.category_code] ?? 'work';
    return {
      id: record.id,
      name: record.name,
      category: record.category_code,
      screen: record.catalog_screen,
      screens: record.screens,
      surface,
      widget: widgetKind(record),
      config_keys: record.config_keys,
      endpoints: record.runtime_endpoints_current ?? [],
      status: record.current_status,
      status_note: record.status_note ?? null,
      behavior: record.behavior,
      default: record.default,
      apply_timing: record.apply_timing,
      acceptance: record.acceptance_scenario_ids,
      feature_refs: record.feature_refs,
      hosts: record.hosts,
      handler: `surface:${surface}`,
      observable: observableFor(record, surface),
    };
  });
  const bySurface = {};
  for (const entry of plan) {
    bySurface[entry.surface] = (bySurface[entry.surface] ?? 0) + 1;
  }
  return {
    schema: 'faktor-edition5-control-plan/v1',
    host,
    inputs_digest: registry.inputs_digest,
    total: plan.length,
    by_surface: bySurface,
    entries: plan,
  };
}

function observableFor(record, surface) {
  if (record.current_status === 'missing') {
    return { kind: 'unavailable', contract: record.status_note };
  }
  if (surface === 'work' || surface === 'agents') {
    return { kind: 'state', key: 'work.draft/selection' };
  }
  if (surface === 'models' || surface === 'providers' || surface === 'generation') {
    return { kind: 'dto', key: 'provider/model selection' };
  }
  if (surface === 'context' || surface === 'compaction-memory' || surface === 'instructions') {
    return { kind: 'state', key: 'context/manifest state' };
  }
  if (surface === 'permissions') {
    return { kind: 'state', key: 'permission decision' };
  }
  if (surface === 'tools-mcp' || surface === 'skills-hooks') {
    return { kind: 'state', key: 'bounded configuration/verdict' };
  }
  if (surface === 'budgets') {
    return { kind: 'state', key: 'budget reservation' };
  }
  if (surface === 'changes-proof') {
    return { kind: 'file', key: 'candidate/proof/baseline' };
  }
  if (surface === 'sessions-display') {
    return { kind: 'state', key: 'binding/generation' };
  }
  if (surface === 'terminal-browser') {
    return { kind: 'state', key: 'owned process/output' };
  }
  return { kind: 'state', key: 'settings/diagnostics state' };
}

export function buildRegistry() {
  const controlsText = readFileSync(CONTROLS_PATH, 'utf8');
  const screensText = readFileSync(SCREENS_PATH, 'utf8');
  const flowsText = readFileSync(FLOWS_PATH, 'utf8');
  const acceptanceText = readFileSync(ACCEPTANCE_PATH, 'utf8');
  const controls = JSON.parse(controlsText);
  const screens = parseScreens(screensText);
  const flows = parseFlows(flowsText);
  const acceptance = parseAcceptance(acceptanceText);
  return {
    schema: 'faktor-edition5-registry/v1',
    inputs_digest: sha256(
      [controlsText, screensText, flowsText, acceptanceText].join('\u0000'),
    ),
    controls,
    screens,
    flows,
    acceptance,
  };
}

export function summarizeRegistry(registry = buildRegistry()) {
  const records = registry.controls.records;
  const byStatus = {};
  const byCategory = {};
  for (const record of records) {
    byStatus[record.current_status] = (byStatus[record.current_status] ?? 0) + 1;
    byCategory[record.category_code] = (byCategory[record.category_code] ?? 0) + 1;
  }
  return {
    schema: 'faktor-edition5-registry-summary/v1',
    inputs_digest: registry.inputs_digest,
    controls: records.length,
    screens: registry.screens.length,
    flows: registry.flows.length,
    acceptance: registry.acceptance.length,
    categories: Object.keys(byCategory).length,
    by_status: byStatus,
    by_category: byCategory,
  };
}

export function main(argv = process.argv.slice(2)) {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`edition5 registry: ${problem}`);
    }
    process.exit(1);
  }
  const summary = summarizeRegistry(registry);
  if (argv.includes('--json')) {
    console.log(JSON.stringify(summary, null, 2));
  } else {
    console.log(
      `edition5 registry: ${summary.controls} controls, ${summary.screens} screens, ` +
        `${summary.flows} flows, ${summary.acceptance} acceptance scenarios, ` +
        `${summary.categories} categories (${summary.inputs_digest})`,
    );
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
