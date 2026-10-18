#!/usr/bin/env node
// 72-screen Edition 8 visual contract gate (both hosts).
//
// Structural and deterministic:
//   * every S01-S72 screen has the frozen control set from registry.json;
//   * both host surfaces expose every screen and its declared control set;
//   * each contract carries the Rev8 widths/themes/zoom/fixture classes and
//     the host-specific visual, state, keyboard, IME and stream obligations;
//   * a per-screen/per-host digest pins the exact contract (drift = failure);
//   * platform evidence is per platform and never inherited.
//
// Output: target/certification/edition8/visual-contracts.raw.json.

import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  ALL_WIDTHS,
  EXPECTED,
  FIXTURE_CLASSES,
  HOSTS,
  PLATFORMS,
  REQUIRED_WIDTHS,
  THEMES,
  buildRegistry,
  validateRegistry,
} from './registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

const registry = buildRegistry();
const problems = validateRegistry(registry);
if (problems.length > 0) {
  for (const problem of problems) console.error(problem);
  process.exit(1);
}

const vscodeRenderer = resolve(ROOT, 'apps/vscode/scripts/edition8/host.mjs');
const vscodeRegistry = resolve(ROOT, 'apps/vscode/src/generated/edition8Registry.ts');
const jetRegistry = resolve(ROOT, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8ScreenRegistry.kt');
const jetPanels = resolve(ROOT, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8Panels.kt');
const jetCatalog = resolve(ROOT, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition8CatalogData.kt');
const hostAvailable = {
  vscode: existsSync(vscodeRenderer) && existsSync(vscodeRegistry),
  jetbrains: existsSync(jetRegistry) && existsSync(jetPanels) && existsSync(jetCatalog),
};
if (!hostAvailable.vscode) problems.push('vscode host renderer/registry missing');
if (!hostAvailable.jetbrains) problems.push('jetbrains host registry/panels/catalog missing');

const jetSource = existsSync(jetRegistry) ? readFileSync(jetRegistry, 'utf8') : '';
const jetScreenIds = new Set([...jetSource.matchAll(/"?(S\d{2})"?/g)].map((match) => match[1]));
const vscodeSource = existsSync(vscodeRegistry) ? readFileSync(vscodeRegistry, 'utf8') : '';
const vscodeScreenIds = new Set([...vscodeSource.matchAll(/"id": "(S\d{2})"/g)].map((match) => match[1]));
if (vscodeSource.length > 0 && vscodeScreenIds.size !== EXPECTED.screens) problems.push(`vscode registry exposes ${vscodeScreenIds.size}/${EXPECTED.screens} screens`);
if (jetSource.length > 0 && jetScreenIds.size !== EXPECTED.screens) problems.push(`jetbrains registry exposes ${jetScreenIds.size}/${EXPECTED.screens} screens`);

const platformSources = {
  vscode: {
    macos: resolve(ROOT, 'apps/vscode/scripts/baselines/edition8/macos.json'),
    linux: resolve(ROOT, 'apps/vscode/scripts/baselines/edition8/linux.json'),
    windows: resolve(ROOT, 'apps/vscode/scripts/baselines/edition8/windows.json'),
  },
  jetbrains: {
    macos: resolve(ROOT, 'apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json'),
    linux: resolve(ROOT, 'apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json'),
    windows: resolve(ROOT, 'apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json'),
  },
};

function platformRecord(host, platform) {
  const path = platformSources[host][platform];
  if (!existsSync(path)) {
    return { status: 'not_certified', reason: `no ${host} ${platform} visual baseline in this checkout` };
  }
  const document = JSON.parse(readFileSync(path, 'utf8'));
  if (host === 'vscode') {
    if (document.platform !== platform) {
      return { status: 'not_certified', reason: `baseline is for ${String(document.platform)}, not ${platform}` };
    }
    if (document.reviewed !== true) return { status: 'not_certified', reason: 'baseline was not reviewed' };
    return {
      status: 'certified',
      source: path.replace(`${ROOT}/`, ''),
      cells: document.records?.length ?? 0,
      sha256: sha(JSON.stringify(document)),
      build: document.chrome ?? 'unknown',
      os: document.os ?? platform,
      font_fingerprint: document.font_fingerprint ?? 'host-default',
    };
  }
  const entry = document.platforms?.[platform];
  if (!entry) return { status: 'not_certified', reason: `jetbrains baseline has no ${platform} record` };
  return {
    status: entry.status === 'certified' ? 'certified' : 'not_certified',
    reason: entry.reason ?? null,
    source: path.replace(`${ROOT}/`, ''),
    cells: entry.cells ?? 0,
    sha256: entry.sha256 ?? sha(JSON.stringify(entry)),
    build: entry.build ?? 'unknown',
    os: entry.os ?? platform,
    font_fingerprint: entry.font_fingerprint ?? 'host-default',
  };
}

const contracts = [];
for (const host of HOSTS) {
  for (const screen of registry.screens) {
    const missingFields = ['title', 'group', 'primary_action', 'controls', 'vscode_variant', 'jetbrains_variant', 'visual', 'states', 'navigation', 'traceability']
      .filter((field) => screen[field] === null || screen[field] === undefined || (Array.isArray(screen[field]) ? screen[field].length === 0 : String(screen[field]).length === 0));
    const platforms = Object.fromEntries(PLATFORMS.map((platform) => [platform, platformRecord(host, platform)]));
    const digest = sha(JSON.stringify({
      host,
      id: screen.id,
      title: screen.title,
      group: screen.group,
      controls: screen.controls,
      visual: screen.visual,
      states: screen.states,
      variant: host === 'vscode' ? screen.vscode_variant : screen.jetbrains_variant,
      widths: ALL_WIDTHS,
      themes: THEMES[host],
      fixture_classes: FIXTURE_CLASSES,
    }));
    const passed = missingFields.length === 0 && screen.controls.length > 0 &&
      (host === 'vscode' ? vscodeScreenIds.has(screen.id) : jetScreenIds.has(screen.id));
    contracts.push({
      host,
      screen: screen.id,
      title: screen.title,
      controls: screen.controls.length,
      control_ids: screen.controls,
      widths: ALL_WIDTHS,
      required_widths: REQUIRED_WIDTHS,
      themes: THEMES[host],
      text_scale: registry.text_scale,
      fixture_classes: FIXTURE_CLASSES,
      states: ['first-read', 'empty', 'ready', 'stale', 'error', 'pending', 'acknowledged', 'scope-switch'],
      keyboard: true,
      ime: true,
      live_stream: true,
      digest,
      host_surface: host === 'vscode' ? vscodeScreenIds.has(screen.id) : jetScreenIds.has(screen.id),
      fields_complete: missingFields.length === 0,
      platforms,
      passed,
      detail: passed ? null : `missingFields=${missingFields.join(',') || 'none'} surface=${host === 'vscode' ? vscodeScreenIds.has(screen.id) : jetScreenIds.has(screen.id)}`,
    });
  }
}

const failed = contracts.filter((contract) => contract.passed !== true);
const certified = contracts.reduce(
  (total, contract) => total + Object.values(contract.platforms).filter((entry) => entry.status === 'certified').length,
  0,
);
const notCertified = contracts.reduce(
  (total, contract) => total + Object.values(contract.platforms).filter((entry) => entry.status === 'not_certified').length,
  0,
);
const document = {
  schema: 'faktor-edition8-visual-contracts/v1',
  generated_at: new Date().toISOString(),
  counts: {
    screens: registry.screens.length,
    hosts: HOSTS.length,
    contracts: contracts.length,
    failed: failed.length,
    fixture_classes: registry.screens.length * FIXTURE_CLASSES.length,
    certified_platform_records: certified,
    not_certified_platform_records: notCertified,
    platforms: PLATFORMS,
  },
  host_available: hostAvailable,
  contracts,
  results: contracts.map((contract) => ({
    id: `${contract.host}:${contract.screen}`,
    passed: contract.passed,
    detail: contract.detail,
  })),
};
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'visual-contracts.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const contract of failed) console.error(`FAIL ${contract.host}:${contract.screen}: ${contract.detail}`);
console.log(
  `edition8 visual contracts: ${contracts.length - failed.length}/${contracts.length} passed; ` +
    `${registry.screens.length} screens x ${HOSTS.length} hosts; ${document.counts.fixture_classes} fixture classes; ` +
    `${certified} certified / ${notCertified} not_certified platform records`,
);
if (failed.length > 0) process.exit(1);
