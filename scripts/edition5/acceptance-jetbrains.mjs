#!/usr/bin/env node
// JetBrains acceptance runner: merges the real host observation overlays
// produced by Edition5AcceptanceSmoke.kt (protocol parsers, MicroMoney,
// host-matrix evidence) onto the shared deterministic observation bundle and
// runs the SAME A01-A40 assertions the VS Code host runs.
//
// A missing overlay file is a failure: the JetBrains host must have executed
// its probe, not borrowed the VS Code observations.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runScenarios } from '../../tests/edition5/acceptance.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const FIXTURES = resolve(ROOT, 'tests/edition5/fixtures/acceptance-observations.json');
const OVERLAYS = resolve(ROOT, 'target/certification/edition5/acceptance-jetbrains.overlays.json');
const OUT = resolve(ROOT, 'target/certification/edition5');

function merge(base, overlay) {
  if (
    base === null ||
    overlay === null ||
    typeof base !== 'object' ||
    typeof overlay !== 'object' ||
    Array.isArray(base) ||
    Array.isArray(overlay)
  ) {
    return overlay;
  }
  const result = { ...base };
  for (const [key, value] of Object.entries(overlay)) {
    result[key] = key in result ? merge(result[key], value) : value;
  }
  return result;
}

if (!existsSync(OVERLAYS)) {
  console.error(
    `jetbrains acceptance: missing ${OVERLAYS}; run Edition5AcceptanceSmoke (compile-and-smoke.sh --smoke Acceptance) first`,
  );
  process.exit(1);
}

const base = JSON.parse(readFileSync(FIXTURES, 'utf8'));
const overlay = JSON.parse(readFileSync(OVERLAYS, 'utf8'));
if (overlay.host !== 'jetbrains') {
  console.error(`jetbrains acceptance: overlay host is ${String(overlay.host)}`);
  process.exit(1);
}
const observations = { ...base };
let applied = 0;
for (const [key, value] of Object.entries(overlay)) {
  if (!/^A\d{2}$/.test(key)) {
    continue;
  }
  observations[key] = merge(base[key] ?? {}, value);
  applied += 1;
}
const results = runScenarios(observations, 'jetbrains');
const failed = results.filter((row) => row.status !== 'passed');
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'acceptance-jetbrains.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-acceptance/v1',
      host: 'jetbrains',
      overlays_applied: applied,
      host_matrix_present: overlay.host_matrix_present === true,
      generated_at: new Date().toISOString(),
      counts: {
        scenarios: results.length,
        passed: results.length - failed.length,
        failed: failed.length,
      },
      results,
    },
    null,
    2,
  )}\n`,
);
for (const row of failed) {
  console.error(`FAIL ${row.id} ${row.title}: ${row.detail}`);
}
console.log(
  `jetbrains acceptance: ${results.length - failed.length}/${results.length} scenarios passed ` +
    `(${applied} host observation overlays applied, host matrix ${overlay.host_matrix_present === true ? 'present' : 'absent'})`,
);
if (failed.length > 0 || applied !== 40) {
  process.exit(1);
}
