#!/usr/bin/env node
// Generates src/generated/edition8Registry.ts from the frozen Revision 8
// artifacts. The registry is data, never behavior: every screen, control,
// workflow and acceptance row is copied from the spec so the UI cannot drift
// from the contracts it claims to implement.
//
// Usage: node scripts/generate-edition8.mjs [--check]

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const SPEC = join(ROOT, '..', '..', 'docs', 'specs', 'edition8');
const OUT = join(ROOT, 'src', 'generated', 'edition8Registry.ts');

const controlsDoc = JSON.parse(readFileSync(join(SPEC, 'controls.json'), 'utf8'));
const screensDoc = readFileSync(join(SPEC, 'screens.md'), 'utf8');
const flowsDoc = readFileSync(join(SPEC, 'flows.md'), 'utf8');
const acceptanceDoc = readFileSync(join(SPEC, 'acceptance.md'), 'utf8');

function clean(value) {
  return value.replace(/\s+/g, ' ').trim();
}

function field(text, label) {
  const pattern = new RegExp(`\\*\\*${label.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}[^*]*\\*\\*\\s*([\\s\\S]*?)(?:\\n\\*\\*|\\n###|\\n##|$)`);
  const match = pattern.exec(text);
  return match === null ? '' : clean(match[1]);
}

function backticked(text) {
  return [...text.matchAll(/`([^`]+)`/g)].map((match) => match[1]);
}

function splitIds(text) {
  return backticked(text)
    .flatMap((entry) => entry.trim().split(/\s+/))
    .filter((entry) => /^[A-Z]-\d{2}$/.test(entry));
}

// ------------------------------------------------------------- screens

const screens = [];
const sectionPattern = /^### (S\d{2}) — (.*)$/gm;
const sections = [];
let match;
while ((match = sectionPattern.exec(screensDoc)) !== null) {
  sections.push({ id: match[1], title: match[2].trim(), start: match.index });
}
for (let index = 0; index < sections.length; index += 1) {
  const section = sections[index];
  const end = index + 1 < sections.length ? sections[index + 1].start : screensDoc.length;
  const text = screensDoc.slice(section.start, end);
  const trace = field(text, 'Traceability');
  const design = /Design p\.(\d+)/.exec(trace);
  const contract = /contract p\.(\d+)/.exec(trace);
  const groupJob = field(text, 'Group / job');
  const [group, ...jobParts] = groupJob.split('.');
  const host = field(text, 'Host variants');
  const vsIndex = host.indexOf('VS Code:');
  const jbIndex = host.indexOf('JetBrains:');
  const vscodeVariant = vsIndex >= 0 ? host.slice(vsIndex + 8, jbIndex >= 0 ? jbIndex : undefined).trim() : '';
  const jetbrainsVariant = jbIndex >= 0 ? host.slice(jbIndex + 9).trim() : '';
  const visual = field(text, 'Visual');
  const surfaceText = vscodeVariant.toUpperCase();
  let surface = 'sidebar';
  if (surfaceText.includes('EDITOR TAB') || surfaceText.includes('WEBVIEW SETTINGS')) surface = 'settings';
  else if (surfaceText.includes('MODAL')) surface = 'modal';
  else if (surfaceText.includes('QUICK PICK')) surface = 'quickpick';
  else if (surfaceText.includes('SETTINGS EDITOR')) surface = 'native-settings';
  else if (surfaceText.includes('DIFF')) surface = 'diff';
  else if (surfaceText.includes('PANEL TERMINAL')) surface = 'terminal';
  else if (surfaceText.includes('SIMPLE BROWSER')) surface = 'browser';
  else if (surfaceText.includes('320 PX')) surface = 'narrow';
  screens.push({
    id: section.id,
    title: section.title,
    group: clean(group),
    job: clean(jobParts.join('.')),
    action: field(text, 'Primary action'),
    controls: splitIds(field(text, 'Controls')),
    layoutRegions: field(text, 'Layout regions'),
    vscodeVariant,
    jetbrainsVariant,
    surface,
    visual,
    states: field(text, 'States'),
    navigation: field(text, 'Navigation'),
    traceability: trace,
    designPage: design === null ? 0 : Number(design[1]),
    contractPage: contract === null ? 0 : Number(contract[1]),
  });
}

if (screens.length !== 72) {
  throw new Error(`expected 72 screens, parsed ${screens.length}`);
}

// ------------------------------------------------------------- controls

const controls = controlsDoc.records.map((record) => ({
  id: record.id,
  category: record.category,
  categoryCode: record.category_code,
  name: record.name,
  control: record.control,
  hosts: record.hosts,
  hostApplicability: record.host_applicability,
  hostNote: record.host_note,
  screens: record.screens,
  primaryScreen: record.primary_screen,
  defaultState: record.default,
  scope: record.scope,
  applyTiming: record.apply_timing,
  behavior: record.behavior,
  emptyState: record.states.empty,
  errorState: record.states.error,
  disabledState: record.states.disabled,
  loadingState: record.states.loading,
  staleState: record.states.stale,
  pendingState: record.states.pending,
  classification: record.classification,
  classificationMeaning: record.classification_meaning,
  buildPackages: record.build_packages,
  configKeys: record.config_keys,
  runtimeEndpoints: record.runtime_endpoints,
  runtimeContractsRequired: record.runtime_contracts_required,
  acceptanceIds: record.acceptance_scenario_ids,
  specPage: record.spec_page,
  currentStatus: record.current_status,
  statusNote: record.status_note,
  contractGap: record.contract_gap,
}));

if (controls.length !== 200) {
  throw new Error(`expected 200 controls, parsed ${controls.length}`);
}

const controlIds = new Set(controls.map((control) => control.id));
for (const screen of screens) {
  for (const id of screen.controls) {
    if (!controlIds.has(id)) throw new Error(`${screen.id} references unknown control ${id}`);
  }
}
for (const control of controls) {
  for (const screen of control.screens) {
    const row = screens.find((entry) => entry.id === screen);
    if (row === undefined) throw new Error(`${control.id} references unknown screen ${screen}`);
    if (!row.controls.includes(control.id)) {
      throw new Error(`${control.id}/${screen}: control lists the screen but the screen footer does not`);
    }
  }
}
const edges = screens.reduce((sum, screen) => sum + screen.controls.length, 0);
if (edges !== 405) throw new Error(`expected 405 control-screen edges, parsed ${edges}`);

// ----------------------------------------------------------- workflows

const workflows = [];
const flowPattern = /^## (W\d{2}) — (.*)$/gm;
const flowSections = [];
while ((match = flowPattern.exec(flowsDoc)) !== null) {
  flowSections.push({ id: match[1], title: match[2].trim(), start: match.index });
}
for (let index = 0; index < flowSections.length; index += 1) {
  const section = flowSections[index];
  const end = index + 1 < flowSections.length ? flowSections[index + 1].start : flowsDoc.length;
  const text = flowsDoc.slice(section.start, end);
  const pathText = field(text, 'Screen path');
  const stepsBlock = /\*\*Steps\.\*\*\s*([\s\S]*?)(?:\n\*\*State transitions|\n\*\*Failure paths|\n##|$)/.exec(text);
  const stepText = stepsBlock === null ? '' : stepsBlock[1];
  const steps = [...stepText.matchAll(/^\d+\.\s+(.+)$/gm)].map((entry) => clean(entry[1]));
  const screensInPath = [...pathText.matchAll(/S\d{2}/g)].map((entry) => entry[0]);
  workflows.push({
    id: section.id,
    title: section.title,
    screenPath: screensInPath,
    entry: field(text, 'Entry'),
    preconditions: field(text, 'Preconditions'),
    steps,
    failurePaths: field(text, 'Failure paths'),
    expected: field(text, 'Expected observable results'),
    forbidden: field(text, 'Forbidden fake states'),
    completeWhen: field(text, 'Complete when'),
    traceability: field(text, 'Traceability'),
    cells: ['pointer', 'keyboard', 'screen_reader', 'compact_zoom', 'recovery'],
  });
}
if (workflows.length !== 16) throw new Error(`expected 16 workflows, parsed ${workflows.length}`);

// ---------------------------------------------------------- acceptance

const acceptance = [];
const aPattern = /^### (A\d{2}) — (.*)$/gm;
const aSections = [];
while ((match = aPattern.exec(acceptanceDoc)) !== null) {
  aSections.push({ id: match[1], title: match[2].trim(), start: match.index });
}
for (let index = 0; index < aSections.length; index += 1) {
  const section = aSections[index];
  const end = index + 1 < aSections.length ? aSections[index + 1].start : acceptanceDoc.length;
  const text = acceptanceDoc.slice(section.start, end);
  const printedMatch = /\*\*Printed requirement[^*]*\*\*\s*"([^"]+)"/.exec(text);
  const preMatch = /\*\*Preconditions \(derived\):\*\*\s*([\s\S]*?)(?:\n\*\*Steps|\n\*\*Expected|\n\*\*Forbidden|\n\*\*Applies|$)/.exec(text);
  const stepsMatch = /\*\*Steps \(derived\):\*\*\s*([\s\S]*?)(?:\n\*\*Expected|\n\*\*Forbidden|\n\*\*Applies|$)/.exec(text);
  const expectedMatch = /\*\*Expected visible \/ field checks:\*\*\s*([\s\S]*?)(?:\n\*\*Forbidden|\n\*\*Applies|\n###|$)/.exec(text);
  const forbiddenMatch = /\*\*Forbidden fake states:\*\*\s*([\s\S]*?)(?:\n\*\*Applies|\n###|$)/.exec(text);
  const appliesMatch = /\*\*Applies:\*\*\s*([^\n]+)/.exec(text);
  const pre = preMatch === null ? '' : clean(preMatch[1]);
  const steps = stepsMatch === null ? '' : clean(stepsMatch[1]);
  acceptance.push({
    id: section.id,
    title: section.title,
    printed: printedMatch === null ? '' : clean(printedMatch[1]),
    preconditions: pre,
    steps,
    expected: expectedMatch === null ? '' : clean(expectedMatch[1]),
    forbidden: forbiddenMatch === null ? '' : clean(forbiddenMatch[1]),
    applies: appliesMatch === null ? '' : clean(appliesMatch[1]),
  });
}
if (acceptance.length !== 40) throw new Error(`expected 40 acceptance scenarios, parsed ${acceptance.length}`);

// ------------------------------------------------------------ categories

const categoryCodes = [...new Set(controls.map((control) => control.categoryCode))];
const categories = categoryCodes.map((code) => {
  const rows = controls.filter((control) => control.categoryCode === code);
  return { code, name: rows[0].category, controls: rows.map((row) => row.id) };
});

const payload = `// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: docs/specs/edition8/{controls.json,screens.md,flows.md,acceptance.md}
// Regenerate: node scripts/generate-edition8.mjs
//
// This registry is the frozen Revision 8 contract data. Behavior lives in
// src/edition8/*; the registry only answers "what does the spec say".

export type E8Surface =
  | 'sidebar'
  | 'settings'
  | 'modal'
  | 'quickpick'
  | 'native-settings'
  | 'diff'
  | 'terminal'
  | 'browser'
  | 'narrow';

export interface E8Control {
  readonly id: string;
  readonly category: string;
  readonly categoryCode: string;
  readonly name: string;
  readonly control: string;
  readonly hosts: readonly string[];
  readonly hostApplicability: string;
  readonly hostNote: string;
  readonly screens: readonly string[];
  readonly primaryScreen: string;
  readonly defaultState: string;
  readonly scope: string;
  readonly applyTiming: string;
  readonly behavior: string;
  readonly emptyState: string;
  readonly errorState: string;
  readonly disabledState: string;
  readonly loadingState: string;
  readonly staleState: string;
  readonly pendingState: string;
  readonly classification: string;
  readonly classificationMeaning: string;
  readonly buildPackages: readonly string[];
  readonly configKeys: readonly string[];
  readonly runtimeEndpoints: readonly { readonly declared?: string; readonly state?: string; readonly actual?: string | readonly string[]; readonly line?: number; readonly lines?: readonly number[] }[];
  readonly runtimeContractsRequired: readonly string[];
  readonly acceptanceIds: readonly string[];
  readonly specPage: number;
  readonly currentStatus: string;
  readonly statusNote: string;
  readonly contractGap: string;
}

export interface E8Screen {
  readonly id: string;
  readonly title: string;
  readonly group: string;
  readonly job: string;
  readonly action: string;
  readonly controls: readonly string[];
  readonly layoutRegions: string;
  readonly vscodeVariant: string;
  readonly jetbrainsVariant: string;
  readonly surface: E8Surface;
  readonly visual: string;
  readonly states: string;
  readonly navigation: string;
  readonly traceability: string;
  readonly designPage: number;
  readonly contractPage: number;
}

export interface E8Category {
  readonly code: string;
  readonly name: string;
  readonly controls: readonly string[];
}

export interface E8Workflow {
  readonly id: string;
  readonly title: string;
  readonly screenPath: readonly string[];
  readonly entry: string;
  readonly preconditions: string;
  readonly steps: readonly string[];
  readonly failurePaths: string;
  readonly expected: string;
  readonly forbidden: string;
  readonly completeWhen: string;
  readonly traceability: string;
  readonly cells: readonly string[];
}

export interface E8Acceptance {
  readonly id: string;
  readonly title: string;
  readonly printed: string;
  readonly preconditions: string;
  readonly steps: string;
  readonly expected: string;
  readonly forbidden: string;
  readonly applies: string;
}

export const E8_CONTROLS: readonly E8Control[] = ${JSON.stringify(controls, null, 2)};

export const E8_SCREENS: readonly E8Screen[] = ${JSON.stringify(screens, null, 2)};

export const E8_CATEGORIES: readonly E8Category[] = ${JSON.stringify(categories, null, 2)};

export const E8_WORKFLOWS: readonly E8Workflow[] = ${JSON.stringify(workflows, null, 2)};

export const E8_ACCEPTANCE: readonly E8Acceptance[] = ${JSON.stringify(acceptance, null, 2)};

export const E8_CONTROL_BY_ID: ReadonlyMap<string, E8Control> = new Map(
  E8_CONTROLS.map((control) => [control.id, control]),
);

export const E8_SCREEN_BY_ID: ReadonlyMap<string, E8Screen> = new Map(
  E8_SCREENS.map((screen) => [screen.id, screen]),
);

export const E8_COUNTS = Object.freeze({
  screens: E8_SCREENS.length,
  controls: E8_CONTROLS.length,
  edges: E8_SCREENS.reduce((sum, screen) => sum + screen.controls.length, 0),
  workflows: E8_WORKFLOWS.length,
  acceptance: E8_ACCEPTANCE.length,
  hostContracts: E8_SCREENS.length * 2,
});
`;

const check = process.argv.includes('--check');
if (check) {
  let current = '';
  try {
    current = readFileSync(OUT, 'utf8');
  } catch {
    current = '';
  }
  if (current !== payload) {
    console.error('edition8 registry is stale; run node scripts/generate-edition8.mjs');
    process.exit(1);
  }
  console.log('edition8 registry is current (72 screens, 200 controls, 405 edges, 16 workflows, 40 acceptance)');
} else {
  writeFileSync(OUT, payload);
  console.log(`wrote ${OUT} (${screens.length} screens, ${controls.length} controls, ${edges} edges, ${workflows.length} flows, ${acceptance.length} scenarios)`);
}
