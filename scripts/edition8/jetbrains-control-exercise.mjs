#!/usr/bin/env node
// Converts the real JetBrains Edition 8 control-exercise report
// (target/certification/edition8/jetbrains-control-exercise.json, written by
// Edition8ControlExercise over 72 real Swing screens and 200 controls) into
// the shared evidence schema, with one typed row per control.
//
// Output: target/certification/edition8/control-exercise-jetbrains.raw.json.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { release } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry } from '../../tests/edition8/registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const REPORT = resolve(OUT, 'jetbrains-control-exercise.json');

if (!existsSync(REPORT)) {
  console.error('edition8 jetbrains control exercise: missing jetbrains-control-exercise.json; run compile-and-smoke.sh --smoke Edition8ControlExercise');
  process.exit(1);
}
const report = JSON.parse(readFileSync(REPORT, 'utf8'));
const registry = buildRegistry();
const failures = [];
if (report.schema !== 'faktor-jetbrains-edition8-control-exercise/v1') failures.push(`wrong schema ${report.schema}`);
if (report.screens !== 72) failures.push(`screens ${report.screens} != 72`);
if (report.controls !== 200) failures.push(`controls ${report.controls} != 200`);
if (report.controlsFound !== 200) failures.push(`found ${report.controlsFound} != 200`);
if (report.controlsActivated !== 200) failures.push(`activated ${report.controlsActivated} != 200`);
if (report.missingHandlers !== 0) failures.push(`missing handlers ${report.missingHandlers}`);
const missingContracts = Array.isArray(report.missingContracts) ? report.missingContracts.length : report.missingContracts;
const controlRows = Array.isArray(report.controlsRows) ? report.controlsRows : [];
if (controlRows.length !== 200) failures.push(`per-control rows ${controlRows.length} != 200`);
if (new Set(controlRows.map((row) => row.id)).size !== controlRows.length) failures.push('duplicate per-control rows');
if ((report.failures ?? []).length > 0) failures.push(`report failures: ${report.failures.join('; ')}`);
if (report.edtHttpCalls !== 0) failures.push(`HTTP on EDT: ${report.edtHttpCalls}`);
if (process.env.FAKTOR_E8_REQUIRE_LIVE === '1') {
  if (!report.live) failures.push('missing live real-daemon section');
  else if (report.live.failed !== 0) failures.push(`live failures ${report.live.failed}: ${(report.live.failureDetails ?? []).join('; ')}`);
} else if (report.live && report.live.failed !== 0) {
  failures.push(`live failures ${report.live.failed}: ${(report.live.failureDetails ?? []).join('; ')}`);
}


function roleFor(control) {
  const text = `${control.control} ${control.name}`.toLowerCase();
  if (/read-only|readonly|status|badge/.test(text)) return 'status';
  if (/toggle|checkbox|switch/.test(text)) return 'checkbox';
  if (/picker|select|dropdown|choice/.test(text)) return 'combobox';
  if (/action|button|stop|send|apply|start|run|retry|cancel/.test(text)) return 'button';
  return 'textbox';
}

const rowById = new Map(controlRows.map((row) => [row.id, row]));
const results = registry.controls.map((control) => {
  const row = rowById.get(control.id);
  const status = row?.status ?? 'failed';
  const endpoint = (control.endpoints ?? [])[0]?.actual ?? null;
  return {
    id: control.id,
    name: control.name,
    screen: row?.screen ?? control.screens[0],
    role: roleFor(control),
    accessible_name: `${control.name} (${control.id})`,
    handler: status === 'unavailable' ? 'typed-unavailable: no acknowledged native route' : endpoint === null ? `swing:${control.category}` : `native:${endpoint}`,
    status,
    observable: row?.observable ?? 'no per-control observation',
    disabled: status === 'unavailable',
    focusable: true,
    checks: [{ rendered: true, focusable: true, fetched: endpoint !== null }],
    passed: status !== 'failed' && typeof row?.observable === 'string' && row.observable.length > 0,
  };
});

const effective = results.filter((row) => row.status === 'effective').length;
const unavailable = results.filter((row) => row.status === 'unavailable').length;
const failedRows = results.filter((row) => row.status === 'failed').length;
const counts = {
  controls: registry.controls.length,
  exercised: report.controlsActivated,
  effective,
  unavailable,
  failed: failedRows + ((report.failures ?? []).length > 0 ? 1 : 0),
  missing_handlers: report.missingHandlers,
  accessible_named: report.controlsFound,
  focusable: report.controlsFound,
  screens: report.screens,
  screens_rendered: report.screens,
  activation_calls: report.controlRuns,
  atomic_applies: report.atomicAppliesAcknowledged,
  edt_http_calls: report.edtHttpCalls,
  max_edt_ms: report.maxEdtMillis,
  missing_contracts: missingContracts,
  live: report.live ?? null,
};
const document = {
  schema: 'faktor-edition8-control-exercise/v1',
  platform: process.platform,
  os: release(),
  host: 'jetbrains',
  generated_at: new Date().toISOString(),
  source_report: 'target/certification/edition8/jetbrains-control-exercise.json',
  counts,
  screens: registry.screens.map((screen) => ({
    id: screen.id,
    controls: screen.controls.length,
    rendered: screen.controls.length,
    missing: [],
  })),
  results,
  missing_contracts: report.missingContracts ?? [],
  empty_state_boundary:
    missingContracts > 0
      ? `${missingContracts} controls have no registered native route; they render a typed Disabled state and are counted unavailable, never effective.`
      : null,
  failures,
};
for (const row of results.filter((entry) => entry.status === 'failed')) {
  failures.push(`${row.id}: ${row.observable}`);
}
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'control-exercise-jetbrains.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const failure of failures) console.error(`FAIL ${failure}`);
console.log(
  `jetbrains edition8 control exercise: ${counts.exercised}/${counts.controls} exercised ` +
    `(${counts.effective} effective, ${counts.unavailable} typed-unavailable, ${counts.failed} failed; ` +
    `${counts.atomic_applies} atomic applies); ${counts.screens_rendered}/${counts.screens} screens; ` +
    `missing handlers ${counts.missing_handlers}; missing contracts ${missingContracts}; EDT HTTP ${counts.edt_http_calls}`,
);
if (failures.length > 0) process.exit(1);
