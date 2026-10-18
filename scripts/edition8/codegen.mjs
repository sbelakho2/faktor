#!/usr/bin/env node
// Generates the Edition 8 host registries from the frozen Rev8 artifacts:
//
//   apps/vscode/src/generated/edition8Registry.ts
//   apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8CatalogData.kt
//
// The registries are data, never behavior. Control contracts are the same 200
// rows as Edition 5 (Rev8 changed screen placement, not control semantics), so
// both hosts reuse their existing control catalogs and add only the 72-screen
// placement map.
//
// Usage: node scripts/edition8/codegen.mjs [--write|--check]

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry, validateRegistry } from '../../tests/edition8/registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT_TS = resolve(ROOT, 'apps/vscode/src/generated/edition8Registry.ts');
const OUT_KT = resolve(ROOT, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8CatalogData.kt');

function json(value) {
  return JSON.stringify(value, null, 2);
}

function ts() {
  const registry = buildRegistry();
  const surfaceOf = (variant) => {
    const value = (variant ?? '').toLowerCase();
    if (value.includes('quick pick')) return 'quickpick';
    if (value.includes('modal')) return 'modal';
    if (value.includes('settings editor')) return 'native-settings';
    if (value.includes('diff')) return 'diff';
    if (value.includes('terminal') || value.includes('console')) return 'terminal';
    if (value.includes('browser') || value.includes('jcef')) return 'browser';
    if (value.includes('320 px') || value.includes('narrow')) return 'narrow';
    if (value.includes('editor tab')) return 'settings';
    return 'sidebar';
  };
  const screens = registry.screens.map((screen) => ({
    id: screen.id,
    title: screen.title,
    group: screen.group,
    job: screen.job ?? '',
    action: screen.primary_action ?? '',
    primaryAction: screen.primary_action ?? '',
    controls: screen.controls,
    surface: surfaceOf(screen.vscode_variant),
    designPage: Number((/Design p\.(\d+)/.exec(screen.traceability ?? '') ?? [])[1] ?? 0),
    contractPage: screen.contract_page,
    layoutRegions: screen.layout_regions ?? '',
    vscodeVariant: screen.vscode_variant ?? '',
    jetbrainsVariant: screen.jetbrains_variant ?? '',
    visual: screen.visual ?? '',
    states: screen.states ?? '',
    navigation: screen.navigation ?? '',
    traceability: screen.traceability ?? '',
  }));
  const controls = registry.controls.map((control) => ({
    id: control.id,
    category: control.category,
    categoryCode: control.category_code,
    name: control.name,
    control: control.control,
    hosts: control.hosts,
    hostApplicability: control.host_applicability,
    screens: control.screens,
    primaryScreen: control.primary_screen,
    defaultState: control.default,
    defaultValue: control.default,
    scope: control.scope,
    applyTiming: control.apply_timing,
    behavior: control.behavior,
    states: control.states,
    emptyState: control.states?.empty ?? '',
    errorState: control.states?.error ?? '',
    disabledState: control.states?.disabled ?? '',
    loadingState: control.states?.loading ?? '',
    staleState: control.states?.stale ?? '',
    pendingState: control.states?.pending ?? '',
    classification: control.classification,
    configKeys: control.config_keys,
    endpoints: control.runtime_endpoints ?? [],
    contractGap: control.contract_gap ?? '',
    currentStatus: control.current_status,
    acceptanceIds: control.acceptance_scenario_ids,
    specPage: control.spec_page,
  }));
  const workflows = registry.workflows.map((workflow) => ({
    id: workflow.id,
    title: workflow.title,
    screens: workflow.screens,
    controls: workflow.controls,
    cells: workflow.cells,
    completeWhen: workflow.complete_when,
    forbiddenFakeStates: workflow.forbidden_fake_states,
  }));
  return `// GENERATED FILE - DO NOT EDIT BY HAND.
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

export const E8_COUNTS = ${json(registry.counts)} as const;

export const E8_WIDTHS = ${json(registry.widths)} as const;
export const E8_TEXT_SCALE = ${json(registry.text_scale)} as const;
export const E8_FIXTURE_CLASSES = ${json(registry.fixture_classes)} as const;
export const E8_THEMES = ${json(registry.themes)} as const;

export const E8_SCREENS: readonly Edition8Screen[] = ${json(screens)};

export const E8_CONTROLS: readonly Edition8Control[] = ${json(controls)};

export const E8_WORKFLOWS: readonly Edition8Workflow[] = ${json(workflows)};

export const E8_SCREEN_BY_ID: ReadonlyMap<string, Edition8Screen> = new Map(
  E8_SCREENS.map((screen) => [screen.id, screen]),
);

export const E8_CONTROL_BY_ID: ReadonlyMap<string, Edition8Control> = new Map(
  E8_CONTROLS.map((control) => [control.id, control]),
);
`;
}

function ktChunks(rows) {
  const chunks = [];
  let current = '';
  for (const row of rows) {
    const line = `${row}\n`;
    if (Buffer.byteLength(current + line, 'utf8') > 50000) {
      chunks.push(current);
      current = '';
    }
    current += line;
  }
  if (current.length > 0) chunks.push(current);
  return chunks;
}

function kt() {
  const registry = buildRegistry();
  const rows = registry.screens.map((screen) =>
    [
      screen.id,
      screen.title,
      screen.group,
      screen.primary_action ?? '',
      screen.controls.join(' '),
      (screen.vscode_variant ?? '').replace(/\t/g, ' '),
      (screen.jetbrains_variant ?? '').replace(/\t/g, ' '),
      screen.contract_page,
    ].join('\t'),
  );
  const chunks = ktChunks(rows);
  return `// GENERATED from docs/specs/edition8/registry.json (72 Rev8 screens).
// Chunked because a single JVM UTF-8 constant is limited to 65535 bytes.
package dev.faktor.frontend

internal object Edition8CatalogData {
    private val CHUNKS: List<String> = listOf(
${chunks.map((chunk) => `        """\n${chunk}""",`).join('\n')}
    )

    val RECORDS: List<String> = CHUNKS
}
`;
}

const generation = { [OUT_TS]: ts(), [OUT_KT]: kt() };
const check = !process.argv.includes('--write');

let failed = false;
const registry = buildRegistry();
const problems = validateRegistry(registry);
if (problems.length > 0) {
  for (const problem of problems) console.error(problem);
  process.exit(1);
}
for (const [path, body] of Object.entries(generation)) {
  if (check) {
    const existing = existsSync(path) ? readFileSync(path, 'utf8') : null;
    if (existing !== body) {
      console.error(`edition8 codegen: ${path} is stale; run node scripts/edition8/codegen.mjs --write`);
      failed = true;
    }
  } else {
    writeFileSync(path, body);
    console.log(`edition8 codegen: wrote ${path}`);
  }
}
if (failed) process.exit(1);
if (check) {
  console.log(`edition8 codegen: both host registries are current (${registry.screens.length} screens, ${registry.controls.length} controls)`);
}
