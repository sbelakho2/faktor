#!/usr/bin/env node
// Aggregated rendered-UI contrast gate for both hosts.
// Text: 4.5:1 normal / 3:1 large. Faktor-drawn graphics: 3:1. Host-default
// tokens below threshold are reported, never used as a waiver.

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

const vscode = read('contrast-vscode.raw.json');
const jetbrains = read('contrast-jetbrains.raw.json');
const problems = [];
const findings = {};
for (const [host, document] of [['vscode', vscode], ['jetbrains', jetbrains]]) {
  if (document === null) {
    problems.push(`missing ${host} contrast evidence`);
    continue;
  }
  const failures = (document.failures ?? []).filter((failure) => typeof failure === 'object');
  const drawn = document.counts?.faktor_drawn_failures ?? failures.length;
  if (drawn !== 0) problems.push(`${host}: ${drawn} Faktor-drawn pairs below threshold`);
  findings[host] = {
    measured_pairs: document.counts?.measured_pairs ?? document.counts?.pairs ?? 0,
    host_default_findings: (document.host_default_findings ?? []).length + (document.counts?.host_default_failures ?? 0),
    themes: [...new Set((document.pairs ?? []).map((pair) => pair.theme))].length,
  };
}
const measured = Object.values(findings).reduce((total, entry) => total + entry.measured_pairs, 0);
if (measured < 80) problems.push(`measured ${measured} distinct pairs < 80`);
const document = {
  schema: 'faktor-edition8-contrast-gate/v1',
  generated_at: new Date().toISOString(),
  method: 'WCAG 2.2 AA from rendered computed styles (VS Code) and real Swing colours (JetBrains)',
  counts: {
    pairs: 80,
    measured_pairs: measured,
    failed: problems.length,
    faktor_drawn_failures: problems.filter((problem) => problem.includes('Faktor-drawn')).length,
    host_default_findings: Object.values(findings).reduce((total, entry) => total + entry.host_default_findings, 0),
    hosts: Object.keys(findings).length,
  },
  findings,
  failures: problems,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'contrast.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const problem of problems) console.error(`FAIL ${problem}`);
console.log(
  `edition8 contrast: 80-pair ledger; ${measured} distinct rendered pairs; ` +
    `${document.counts.host_default_findings} reported host-default findings; ${problems.length} failures`,
);
if (problems.length > 0) process.exit(1);
