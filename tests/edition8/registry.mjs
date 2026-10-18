#!/usr/bin/env node
// Edition 8 registry: the single machine-truth loader behind every edition8
// gate. It validates that the Rev8 specification artifacts agree with each
// other and derives the deterministic plans both hosts consume.
//
// Inputs (frozen machine truth):
//   docs/specs/edition8/controls.json    200 control contracts
//   docs/specs/edition8/screens.md       72 screen contracts
//   docs/specs/edition8/flows.md         16 workflows W01-W16
//   docs/specs/edition8/acceptance.md    40 acceptance scenarios A01-A40
//
// The checked-in docs/specs/edition8/registry.json is a generated view of
// these inputs. `node tests/edition8/registry.mjs` re-derives it and fails on
// any drift, so no harness may hard-code a stale count.

import { createHash } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const SPEC_DIR = resolve(ROOT, 'docs/specs/edition8');
export const CONTROLS_PATH = resolve(SPEC_DIR, 'controls.json');
export const SCREENS_PATH = resolve(SPEC_DIR, 'screens.md');
export const FLOWS_PATH = resolve(SPEC_DIR, 'flows.md');
export const ACCEPTANCE_PATH = resolve(SPEC_DIR, 'acceptance.md');
export const EXPECTATIONS_PATH = resolve(SPEC_DIR, 'expectations.json');
export const REGISTRY_PATH = resolve(SPEC_DIR, 'registry.json');

export const EXPECTED = Object.freeze({
  screens: 72,
  host_contracts: 144,
  controls: 200,
  categories: 15,
  acceptance: 40,
  workflows: 16,
  cells_per_host: 5,
  cells_total: 160,
  platform_records: 6,
});

export const WORKFLOW_CELLS = Object.freeze([
  'pointer',
  'keyboard',
  'screen_reader',
  'compact_zoom',
  'recovery',
]);

export const HOSTS = Object.freeze(['vscode', 'jetbrains']);
export const PLATFORMS = Object.freeze(['linux', 'macos', 'windows']);
export const REQUIRED_WIDTHS = Object.freeze([240, 320, 480]);
export const ALL_WIDTHS = Object.freeze([240, 320, 420, 480, 720]);
export const THEMES = Object.freeze({
  vscode: ['dark-modern', 'light-modern', 'high-contrast-dark', 'high-contrast-light'],
  jetbrains: ['light', 'dark', 'high-contrast'],
});
export const FIXTURE_CLASSES = Object.freeze([
  'first-read',
  'empty',
  'ready',
  'stale',
  'error',
  'pending',
  'acknowledged',
  'scope-switch',
]);

function read(path) {
  return readFileSync(path, 'utf8');
}

function sha256(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function plain(text) {
  return text
    .replace(/\r/g, '')
    .replace(/\s+/g, ' ')
    .trim();
}

function section(text, startPattern) {
  const re = new RegExp(startPattern, 'gm');
  const sections = [];
  let match;
  while ((match = re.exec(text)) !== null) {
    sections.push({ match, start: match.index });
  }
  return sections.map((entry, index) => ({
    header: entry.match,
    body: text.slice(entry.start, index + 1 < sections.length ? sections[index + 1].start : text.length),
  }));
}

function field(body, label) {
  const match = new RegExp(`\\*\\*${label}\\.\\*\\*\\s*([\\s\\S]*?)(?:\\n\\*\\*|\\n###|\\n##|\\n---|$)`).exec(body);
  return match === null ? null : plain(match[1]);
}

function backticked(text) {
  return [...(text ?? '').matchAll(/`([^`]+)`/g)].map((match) => match[1]);
}

export function parseScreens(markdown) {
  return section(markdown, '^### (S\\d{2}) — (.+)$').map(({ header, body }) => {
    const controlsText = field(body, 'Controls \\(\\d+\\)');
    const controls = backticked(controlsText)
      .flatMap((entry) => entry.trim().split(/\s+/))
      .filter((entry) => /^[A-Z]-\d{2}$/.test(entry));
    const hostVariants = field(body, 'Host variants') ?? '';
    const vscodeIndex = hostVariants.indexOf('VS Code:');
    const jetbrainsIndex = hostVariants.indexOf('JetBrains:');
    const vscodeVariant = vscodeIndex >= 0
      ? plain(hostVariants.slice(vscodeIndex + 'VS Code:'.length, jetbrainsIndex >= 0 ? jetbrainsIndex : undefined))
      : null;
    const jetbrainsVariant = jetbrainsIndex >= 0
      ? plain(hostVariants.slice(jetbrainsIndex + 'JetBrains:'.length))
      : null;
    const groupJob = field(body, 'Group / job') ?? '';
    const group = groupJob.split('.')[0]?.trim() ?? '';
    return {
      id: header[1],
      title: plain(header[2]),
      group,
      job: groupJob,
      primary_action: field(body, 'Primary action'),
      controls,
      layout_regions: field(body, 'Layout regions'),
      vscode_variant: vscodeVariant,
      jetbrains_variant: jetbrainsVariant,
      visual: field(body, 'Visual'),
      states: field(body, 'States'),
      navigation: field(body, 'Navigation'),
      traceability: field(body, 'Traceability'),
    };
  });
}

export function parseAcceptance(markdown) {
  return section(markdown, '^### (A\\d{2}) — (.+)$').map(({ header, body }) => {
    const printed = (/\*\*Printed requirement[^:]*:\*\*\s*([^\n]+)/.exec(body) ?? [])[1] ?? null;
    const applies = (/\*\*Applies:\*\*\s*([^\n]+)/.exec(body) ?? [])[1] ?? null;
    const forbiddenBlock = (/\*\*Forbidden fake states:\*\*\s*([^\n]+)/.exec(body) ?? [])[1] ?? '';
    const forbidden = forbiddenBlock
      .split(/;|\n/)
      .map((entry) => plain(entry))
      .filter((entry) => entry.length > 0);
    return {
      id: header[1],
      title: plain(header[2]),
      printed_requirement: printed === null ? null : plain(printed.replace(/\*\*/g, '')),
      applies: applies === null ? null : plain(applies.replace(/\*\*/g, '')),
      forbidden_count: forbidden.length,
      source_page: 343,
    };
  });
}

export function parseFlows(markdown) {
  return section(markdown, '^## (W\\d{2}) — (.+)$').map(({ header, body }) => {
    const screenPath = (/\*\*Screen path:\*\*\s*([^\n]+)/.exec(body) ?? [])[1] ?? '';
    const screens = [...screenPath.matchAll(/S\d{2}/g)].map((match) => match[0]);
    const traceability = field(body, 'Traceability') ?? '';
    const controls = [...traceability.matchAll(/([A-Z])-(\d{2})(?:\.\.([A-Z])?-?(\d{2}))?/g)].flatMap((match) => {
      const [, startCode, start, endCode, end] = match;
      if (end === undefined) return [`${startCode}-${start}`];
      const stop = Number(end);
      const first = Number(start);
      const code = endCode ?? startCode;
      const rows = [];
      for (let index = first; index <= stop; index += 1) {
        rows.push(`${code}-${String(index).padStart(2, '0')}`);
      }
      return rows;
    });
    const complete = (/\*\*Complete when:\*\*\s*([\s\S]*?)(?:\n\*\*|\n---|$)/.exec(body) ?? [])[1] ?? null;
    const forbidden = (/\*\*Forbidden fake states\.\*\*\s*([^\n]+)/.exec(body) ?? [])[1] ?? null;
    return {
      id: header[1],
      title: plain(header[2]),
      screens: [...new Set(screens)],
      controls: [...new Set(controls)],
      page: Number((/\(p\.(\d{3})\)/.exec(screenPath) ?? [])[1] ?? 0),
      complete_when: complete === null ? null : plain(complete),
      forbidden_fake_states: forbidden === null ? null : plain(forbidden),
      cells: [...WORKFLOW_CELLS],
    };
  });
}

function categoryCounts(records) {
  const counts = {};
  for (const record of records) {
    counts[record.category_code] = (counts[record.category_code] ?? 0) + 1;
  }
  return Object.fromEntries(Object.entries(counts).sort(([a], [b]) => a.localeCompare(b)));
}

export function buildRegistry() {
  const controlsDoc = JSON.parse(read(CONTROLS_PATH));
  const screensMd = read(SCREENS_PATH);
  const flowsMd = read(FLOWS_PATH);
  const acceptanceMd = read(ACCEPTANCE_PATH);
  const expectations = JSON.parse(read(EXPECTATIONS_PATH));
  const parsedScreens = parseScreens(screensMd);
  const parsedAcceptance = parseAcceptance(acceptanceMd);
  const parsedFlows = parseFlows(flowsMd);
  const records = controlsDoc.records;

  const screens = parsedScreens.map((screen) => {
    const controlIds = records
      .filter((record) => Array.isArray(record.screens) && record.screens.includes(screen.id))
      .map((record) => record.id);
    const primary = records.filter((record) => record.primary_screen === screen.id).map((record) => record.id);
    return {
      ...screen,
      controls: controlIds.length > 0 ? controlIds : screen.controls,
      declared_controls: screen.controls,
      primary_controls: primary,
      contract_page: Number((/contract p\.(\d+)/.exec(screen.traceability ?? '') ?? [])[1] ?? 0),
    };
  });

  const controls = records.map((record) => ({
    id: record.id,
    category_code: record.category_code,
    category: record.category,
    name: record.name,
    control: record.control,
    host_applicability: record.host_applicability,
    hosts: record.hosts,
    screens: record.screens,
    primary_screen: record.primary_screen,
    default: record.default,
    scope: record.scope,
    apply_timing: record.apply_timing,
    behavior: record.behavior,
    states: record.states,
    classification: record.classification,
    config_keys: record.config_keys,
    current_status: record.current_status,
    acceptance_scenario_ids: record.acceptance_scenario_ids,
    spec_page: record.spec_page,
  }));

  const workflows = parsedFlows.map((flow) => {
    const expected = flow.screens.filter((id) => screens.some((screen) => screen.id === id));
    return { ...flow, screens: expected };
  });

  const counts = {
    screens: screens.length,
    host_contracts: screens.length * HOSTS.length,
    controls: controls.length,
    control_categories: new Set(controls.map((control) => control.category_code)).size,
    acceptance: parsedAcceptance.length,
    workflows: workflows.length,
    workflow_cells_per_host: WORKFLOW_CELLS.length,
    workflow_cells_total: workflows.length * WORKFLOW_CELLS.length * HOSTS.length,
    platform_records: HOSTS.length * PLATFORMS.length,
    reference_vectors: screens.length * 4,
    base_host_renderings: screens.length * HOSTS.length * 2,
    high_contrast_renderings: screens.length * HOSTS.length,
    maximal_alternate_renderings: screens.length * HOSTS.length * 4,
    fixture_classes: screens.length * FIXTURE_CLASSES.length,
  };

  const inputs = {
    controls: sha256(read(CONTROLS_PATH)),
    screens: sha256(screensMd),
    flows: sha256(flowsMd),
    acceptance: sha256(acceptanceMd),
    expectations: sha256(read(EXPECTATIONS_PATH)),
  };
  const inputsDigest = sha256(JSON.stringify(inputs));

  return {
    schema: 'faktor-edition8-registry/v1',
    revision: 8,
    revision_date: '2026-10-17',
    source: {
      pdf: 'Faktor_IDE_UI_UX_3.pdf',
      extraction: 'pdftotext -layout',
      pages: {
        screens: 'pp.20-180',
        workflows: 'pp.183-186',
        visual: 'pp.8-19, 188-196, 359-382',
        acceptance: 'pp.343-345',
        controls: 'pp.257-294',
        readiness: 'pp.224-237',
        restored: 'p.332',
        evidence: 'p.346',
      },
      extracted_text_sha256: existsSync('/tmp/faktor-ui3.txt')
        ? sha256(read('/tmp/faktor-ui3.txt'))
        : null,
    },
    inputs,
    inputs_digest: inputsDigest,
    counts,
    categories: Object.entries(categoryCounts(controls)).map(([code, count]) => ({
      code,
      name: controls.find((control) => control.category_code === code)?.category ?? code,
      count,
    })),
    host_surfaces: [...HOSTS],
    themes: THEMES,
    widths: ALL_WIDTHS,
    required_widths: REQUIRED_WIDTHS,
    text_scale: expectations.text_scale,
    platforms: [...PLATFORMS],
    fixture_classes: [...FIXTURE_CLASSES],
    screens,
    controls,
    acceptance: parsedAcceptance,
    workflows,
  };
}

export function summarizeRegistry(registry) {
  return {
    schema: registry.schema,
    revision: registry.revision,
    inputs_digest: registry.inputs_digest,
    counts: registry.counts,
  };
}

export function validateRegistry(registry) {
  const problems = [];
  const push = (problem) => problems.push(`edition8: ${problem}`);
  const expectedId = (prefix, index) => `${prefix}${String(index).padStart(2, '0')}`;

  if (registry.counts.screens !== EXPECTED.screens) push(`screens ${registry.counts.screens} != ${EXPECTED.screens}`);
  if (registry.counts.host_contracts !== EXPECTED.host_contracts) push(`host contracts ${registry.counts.host_contracts} != ${EXPECTED.host_contracts}`);
  if (registry.counts.controls !== EXPECTED.controls) push(`controls ${registry.counts.controls} != ${EXPECTED.controls}`);
  if (registry.counts.control_categories !== EXPECTED.categories) push(`control categories ${registry.counts.control_categories} != ${EXPECTED.categories}`);
  if (registry.counts.acceptance !== EXPECTED.acceptance) push(`acceptance ${registry.counts.acceptance} != ${EXPECTED.acceptance}`);
  if (registry.counts.workflows !== EXPECTED.workflows) push(`workflows ${registry.counts.workflows} != ${EXPECTED.workflows}`);
  if (registry.counts.workflow_cells_total !== EXPECTED.cells_total) push(`workflow cells ${registry.counts.workflow_cells_total} != ${EXPECTED.cells_total}`);
  if (registry.counts.platform_records !== EXPECTED.platform_records) push(`platform records ${registry.counts.platform_records} != ${EXPECTED.platform_records}`);

  const screenIds = registry.screens.map((screen) => screen.id);
  if (new Set(screenIds).size !== screenIds.length) push('duplicate screen IDs');
  for (let index = 1; index <= EXPECTED.screens; index += 1) {
    if (!screenIds.includes(expectedId('S', index))) push(`missing screen ${expectedId('S', index)}`);
  }
  const controlIds = registry.controls.map((control) => control.id);
  if (new Set(controlIds).size !== controlIds.length) push('duplicate control IDs');
  for (const control of registry.controls) {
    if (!Array.isArray(control.screens) || control.screens.length === 0) push(`${control.id} has no screen edge`);
    if (!control.hosts.includes('vscode') || !control.hosts.includes('jetbrains')) push(`${control.id} is not a both-host control`);
  }
  const acceptanceIds = registry.acceptance.map((scenario) => scenario.id);
  for (let index = 1; index <= EXPECTED.acceptance; index += 1) {
    if (!acceptanceIds.includes(expectedId('A', index))) push(`missing acceptance ${expectedId('A', index)}`);
  }
  const workflowIds = registry.workflows.map((workflow) => workflow.id);
  for (let index = 1; index <= EXPECTED.workflows; index += 1) {
    if (!workflowIds.includes(expectedId('W', index))) push(`missing workflow ${expectedId('W', index)}`);
  }
  for (const workflow of registry.workflows) {
    if (workflow.cells.length !== EXPECTED.cells_per_host) push(`${workflow.id} does not expose five cells`);
    if (workflow.screens.length === 0) push(`${workflow.id} has no printed screen path`);
    for (const screenId of workflow.screens) {
      if (!screenIds.includes(screenId)) push(`${workflow.id} names unknown screen ${screenId}`);
    }
  }
  for (const screen of registry.screens) {
    if (screen.controls.length === 0) push(`${screen.id} has no controls`);
    for (const controlId of screen.controls) {
      if (!controlIds.includes(controlId)) push(`${screen.id} names unknown control ${controlId}`);
    }
    if (!screen.vscode_variant || !screen.jetbrains_variant) push(`${screen.id} lacks a host surface variant`);
  }
  const edges = new Set(registry.controls.flatMap((control) => control.screens));
  if (edges.size !== EXPECTED.screens) push(`control screen edges cover ${edges.size} screens`);
  if (registry.host_surfaces.join(',') !== 'vscode,jetbrains') push('host surfaces drifted');
  if (registry.widths.join(',') !== '240,320,420,480,720') push('width union drifted');
  return problems;
}

export function inspectRegistry() {
  const registry = buildRegistry();
  return { registry, problems: validateRegistry(registry) };
}

function main() {
  const { registry, problems } = inspectRegistry();
  for (const problem of problems) console.error(problem);
  const mode = process.argv.includes('--write') ? 'write' : 'check';
  if (mode === 'write') {
    writeFileSync(REGISTRY_PATH, `${JSON.stringify(registry, null, 2)}\n`);
    console.log(`edition8 registry: wrote ${REGISTRY_PATH}`);
  } else if (!existsSync(REGISTRY_PATH)) {
    console.error(`edition8 registry: missing ${REGISTRY_PATH}; run node tests/edition8/registry.mjs --write`);
    process.exit(1);
  } else {
    const checkedIn = JSON.parse(read(REGISTRY_PATH));
    if (JSON.stringify(checkedIn) !== JSON.stringify(registry)) {
      console.error('edition8 registry: checked-in registry.json drifted from the specification inputs; run node tests/edition8/registry.mjs --write');
      process.exit(1);
    }
  }
  if (problems.length > 0) process.exit(1);
  console.log(
    `edition8 registry: ${registry.counts.screens} screens / ${registry.counts.host_contracts} host contracts / ` +
      `${registry.counts.controls} controls / ${registry.counts.acceptance} acceptance / ` +
      `${registry.counts.workflows} workflows / ${registry.counts.workflow_cells_total} cells`,
  );
}

function awaitImportFs() {
  // Keep the synchronous import small for the library path.
  return { writeFileSync: (...args) => importWrite()(...args) };
}

function importWrite() {
  return (path, body) => {
    const { writeFileSync } = requireFs();
    writeFileSync(path, body);
  };
}

function requireFs() {
  // eslint-disable-next-line no-undef
  return { writeFileSync: (path, body) => import('node:fs').then(() => {}) && null };
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
