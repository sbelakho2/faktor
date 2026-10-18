#!/usr/bin/env node
// Aggregated optical cap-centre gate for both hosts.
// VS Code measures rasterised 3x button crops; JetBrains measures the real
// Swing button paint. Every host/theme must have >=30 button measurements and
// stay inside the printed max |2.0 px| bound.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');

function read(name) {
  const path = resolve(OUT, name);
  return existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : null;
}

const vscode = read('optical-vscode.raw.json');
const jetbrains = read('optical-jetbrains.raw.json');
const problems = [];
const measurements = {};

// Per-platform host evidence is authoritative where it exists. A Linux run
// must stay in the gate after a macOS run refreshes the generic file.
const platforms = {};
for (const host of ['vscode', 'jetbrains']) {
  for (const platformName of ['linux', 'macos', 'windows']) {
    const document = read(`optical-${host}.${platformName}.raw.json`);
    if (document === null) continue;
    const entries = document.measurements ?? {};
    const buttons = Object.values(entries).reduce((sum, entry) => sum + (entry.buttons ?? 0), 0);
    const failed = document.counts?.failed ?? document.failures?.length ?? 0;
    platforms[`${host}:${platformName}`] = { buttons, themes: Object.keys(entries).length, failed };
    if (failed > 0) problems.push(`${host}/${platformName}: ${failed} optical failures`);
    if (document.platform !== platformName) problems.push(`${host}/${platformName} evidence is labelled ${String(document.platform)}`);
  }
}
for (const [host, document] of [['vscode', vscode], ['jetbrains', jetbrains]]) {
  if (document === null) {
    problems.push(`missing ${host} optical evidence`);
    continue;
  }
  measurements[host] = {};
  const entries = document.measurements ?? {};
  if (Object.keys(entries).length === 0) problems.push(`${host}: no theme measurements`);
  for (const [theme, entry] of Object.entries(entries)) {
    const buttons = entry.buttons ?? 0;
    const median = entry.median ?? 99;
    const maxAbs = entry.max_abs ?? 99;
    measurements[host][theme] = { buttons, median, max_abs: maxAbs };
    if (buttons < 30) problems.push(`${host}/${theme}: ${buttons} buttons < 30`);
    if (Math.abs(median) > 0.5) problems.push(`${host}/${theme}: median ${median} > 0.5 px`);
    if (maxAbs > 2.0) problems.push(`${host}/${theme}: max ${maxAbs} > 2.0 px`);
  }
  for (const failure of document.failures ?? []) problems.push(`${host}: ${failure}`);
}

const document = {
  schema: 'faktor-edition8-optical-gate/v1',
  generated_at: new Date().toISOString(),
  counts: {
    failed: problems.length,
    hosts: Object.keys(measurements).length,
    themes: Object.values(measurements).reduce((total, themes) => total + Object.keys(themes).length, 0),
    buttons: Object.values(measurements).reduce(
      (total, themes) => total + Object.values(themes).reduce((sum, entry) => sum + entry.buttons, 0),
      0,
    ),
  },
  detail: {
    method: 'rasterised ink cap-centre against the real rendered button box; interior crop excludes border and 7 px ends',
    bound: { median_px: 0.5, max_abs_px: 2.0, min_buttons_per_theme: 30 },
    themes: [...new Set(Object.values(measurements).flatMap((themes) => Object.keys(themes)))].sort(),
    measurements,
    platforms,
  },
  results: Object.entries(measurements).flatMap(([host, themes]) =>
    Object.entries(themes).map(([theme, entry]) => ({
      id: `${host}:${theme}`,
      passed: entry.buttons >= 30 && Math.abs(entry.median) <= 0.5 && entry.max_abs <= 2.0,
      ...entry,
    })),
  ),
  failures: problems,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'optical.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const problem of problems) console.error(`FAIL ${problem}`);
console.log(
  `edition8 optical: ${document.counts.buttons} buttons across ${document.counts.hosts} hosts / ${document.counts.themes} theme records; ` +
    `${problems.length} failures`,
);
if (problems.length > 0) process.exit(1);
