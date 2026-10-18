#!/usr/bin/env node
// Generates src/generated/edition5Registry.ts from the frozen Edition 5
// artifacts in docs/specs/edition5. The registry is data, never behavior:
// every control, screen and state string is copied verbatim from the spec
// so the UI cannot drift from the contracts it claims to implement.
//
// Usage: node scripts/generate-edition5.mjs [--check]
//   --check  fail when the checked-in file differs from a fresh render.

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const SPEC = join(ROOT, '..', '..', 'docs', 'specs', 'edition5');
const OUT = join(ROOT, 'src', 'generated', 'edition5Registry.ts');

const controlsDoc = JSON.parse(readFileSync(join(SPEC, 'controls.json'), 'utf8'));
const screensDoc = readFileSync(join(SPEC, 'screens.md'), 'utf8');

function field(text, label) {
  const match = new RegExp(`\\*\\*${label}\\.\\*\\*\\s*([\\s\\S]*?)(?:\\n\\*\\*|\\n##|$)`).exec(text);
  return match === null ? '' : match[1].trim().replace(/\s+/g, ' ');
}

function backticked(text) {
  return [...text.matchAll(/`([^`]+)`/g)].map((match) => match[1]);
}

const screens = [];
const sectionPattern = /^## (S\d{2}) — (.*)$/gm;
const sections = [];
let match;
while ((match = sectionPattern.exec(screensDoc)) !== null) {
  sections.push({ id: match[1], title: match[2].trim(), start: match.index });
}
for (let index = 0; index < sections.length; index += 1) {
  const section = sections[index];
  const end = index + 1 < sections.length ? sections[index + 1].start : screensDoc.length;
  const text = screensDoc.slice(section.start, end);
  screens.push({
    id: section.id,
    title: section.title,
    purpose: field(text, 'Purpose'),
    action: field(text, 'Action'),
    recovery: field(text, 'Recovery'),
    controls: backticked(field(text, 'Controls')).flatMap((entry) => entry.trim().split(/\s+/)).filter((entry) => entry.length > 0),
    layoutRegions: field(text, 'Layout regions'),
    vscodeVariant: field(text, 'VS Code variant'),
    jetbrainsVariant: field(text, 'JetBrains variant'),
    visual: field(text, 'Visual'),
    states: field(text, 'States'),
    navigation: field(text, 'Navigation'),
    traceability: field(text, 'Traceability'),
  });
}

if (screens.length !== 32) {
  throw new Error(`expected 32 screens, parsed ${screens.length}`);
}

const controls = controlsDoc.records.map((record) => ({
  id: record.id,
  category: record.category,
  categoryCode: record.category_code,
  name: record.name,
  control: record.control,
  hosts: record.hosts,
  hostApplicability: record.host_applicability,
  screens: record.screens,
  catalogScreen: record.catalog_screen,
  defaultState: record.default,
  scope: record.scope,
  applyTiming: record.apply_timing,
  behavior: record.behavior,
  emptyState: record.empty_state,
  errorState: record.error_state,
  disabledState: record.disabled_state,
  classification: record.classification,
  buildPackages: record.build_packages,
  configKeys: record.config_keys,
  runtimeRequired: record.runtime_contracts_required,
  runtimeEndpoints: record.runtime_endpoints_current,
  featureRefs: record.feature_refs,
  acceptanceIds: record.acceptance_scenario_ids,
  specPage: record.spec_page,
  currentStatus: record.current_status,
  statusNote: record.status_note,
}));

if (controls.length !== 200) {
  throw new Error(`expected 200 controls, parsed ${controls.length}`);
}

const categoryCodes = [...new Set(controls.map((control) => control.categoryCode))];
const categories = categoryCodes.map((code) => {
  const rows = controls.filter((control) => control.categoryCode === code);
  return {
    code,
    name: rows[0].category,
    controls: rows.map((row) => row.id),
  };
});

const payload = `// GENERATED FILE - DO NOT EDIT BY HAND.
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

export const E5_CONTROLS: readonly Edition5Control[] = ${JSON.stringify(controls, null, 2)};

export const E5_SCREENS: readonly Edition5Screen[] = ${JSON.stringify(screens, null, 2)};

export const E5_CATEGORIES: readonly Edition5Category[] = ${JSON.stringify(categories, null, 2)};

export const E5_CONTROL_BY_ID: ReadonlyMap<string, Edition5Control> = new Map(
  E5_CONTROLS.map((control) => [control.id, control]),
);

export const E5_SCREEN_BY_ID: ReadonlyMap<string, Edition5Screen> = new Map(
  E5_SCREENS.map((screen) => [screen.id, screen]),
);
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
    console.error('edition5 registry is stale; run node scripts/generate-edition5.mjs --write');
    process.exit(1);
  }
  console.log('edition5 registry is current (200 controls, 32 screens)');
} else {
  writeFileSync(OUT, payload);
  console.log(`wrote ${OUT} (${controls.length} controls, ${screens.length} screens)`);
}
