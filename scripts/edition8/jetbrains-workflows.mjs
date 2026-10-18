#!/usr/bin/env node
// JetBrains Edition 8 workflow adapter. Converts the real Swing workflow
// report (16 workflows x 5 cells = 80) into the shared workflow schema.
//
// Output: target/certification/edition8/workflows-jetbrains.raw.json.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { release } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry } from '../../tests/edition8/registry.mjs';
import { CELLS, summarizeWorkflows, validateHostWorkflows } from '../../tests/edition8/workflows.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const REPORT = resolve(OUT, 'workflows-jetbrains-host.json');
const LEGACY = resolve(OUT, 'workflows-jetbrains.json');
const DOGFOOD = resolve(OUT, 'dogfood-jetbrains.raw.json');

if (!existsSync(REPORT)) {
  console.error('edition8 jetbrains workflows: missing workflows-jetbrains-host.json; run compile-and-smoke.sh --smoke Edition8Workflow');
  process.exit(1);
}
const report = JSON.parse(readFileSync(REPORT, 'utf8'));
if (report.schema !== 'faktor-jetbrains-edition8-workflows/v1' && existsSync(LEGACY)) {
  const legacy = JSON.parse(readFileSync(LEGACY, 'utf8'));
  if (legacy.schema === 'faktor-jetbrains-edition8-workflows/v1') {
    console.error('edition8 jetbrains workflows: using legacy host report workflows-jetbrains.json');
  }
}
const registry = buildRegistry();
const problems = [];
if (report.schema !== 'faktor-jetbrains-edition8-workflows/v1') problems.push(`wrong schema ${report.schema}`);
if (report.workflows !== 16) problems.push(`workflows ${report.workflows} != 16`);
if (report.cells !== 80) problems.push(`cells ${report.cells} != 80`);
if (report.passed !== 80) problems.push(`passed ${report.passed} != 80`);
if ((report.failures ?? []).length > 0) problems.push(`failures: ${report.failures.join('; ')}`);

let live = null;
if (existsSync(DOGFOOD)) {
  live = JSON.parse(readFileSync(DOGFOOD, 'utf8'));
  if (live.schema !== 'faktor-edition8-dogfood-host/v1') problems.push(`wrong live schema ${live.schema}`);
  if (live.host !== 'jetbrains') problems.push(`wrong live host ${live.host}`);
  if (live.counts?.failed !== 0) problems.push(`workflow live failures ${live.counts?.failed}`);
  const workflowRows = (live.results ?? []).filter((row) => String(row.id).startsWith('workflow:'));
  if (workflowRows.length < 16) problems.push(`workflow live rows ${workflowRows.length} < 16`);
} else if (process.env.FAKTOR_E8_REQUIRE_LIVE === '1') {
  problems.push('missing dogfood-jetbrains.raw.json; run the real-daemon edition8 dogfood');
}

const byWorkflow = new Map();
for (const record of report.records ?? []) {
  if (!byWorkflow.has(record.workflow)) byWorkflow.set(record.workflow, new Map());
  byWorkflow.get(record.workflow).set(record.cell, record);
}

const results = [];
for (const workflow of registry.workflows) {
  const cells = {};
  for (const cell of CELLS) {
    const record = byWorkflow.get(workflow.id)?.get(cell);
    if (!record) {
      cells[cell] = { status: 'failed', mode: cell, receipt: 'missing host record', reason: 'no Swing observation' };
      problems.push(`${workflow.id} ${cell} missing`);
      continue;
    }
    const passed = record.passed === true && record.result === 'passed';
    if (!passed) problems.push(`${workflow.id} ${cell}: ${record.result}`);
    cells[cell] = {
      status: passed ? 'passed' : 'failed',
      mode: cell === 'screen_reader' ? 'aria_automated' : cell === 'compact_zoom' ? 'layout' : cell,
      receipt: `${workflow.id}/${cell}: ${record.result}`,
      ...(cell === 'screen_reader'
        ? {
            boundary: {
              status: 'not_certified',
              reason: 'No NVDA/VoiceOver/Orca session on this host; the assistive gate records the typed AT boundary.',
            },
          }
        : {}),
    };
  }
  const complete = CELLS.every((cell) => cells[cell].status === 'passed');
  results.push({
    id: workflow.id,
    title: workflow.title,
    host: 'jetbrains',
    platform: process.platform,
    os: release(),
    screens: workflow.screens,
    controls: workflow.controls,
    cells,
    complete_when: complete,
    status: complete ? 'passed' : 'failed',
    detail: complete ? null : 'one or more cells failed',
  });
}

const document = {
  schema: 'faktor-edition8-workflows/v1',
  host: 'jetbrains',
  generated_at: new Date().toISOString(),
  source_report: 'target/certification/edition8/workflows-jetbrains-host.json',
  counts: summarizeWorkflows({ results }, 'jetbrains'),
  live: live === null ? null : live.counts,
  results,
  failures: problems,
};
const validation = validateHostWorkflows(document, 'jetbrains');
problems.push(...validation);
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'workflows-jetbrains.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const problem of problems) console.error(`FAIL ${problem}`);
const passed = results.filter((row) => row.status === 'passed').length;
console.log(
  `jetbrains edition8 workflows: ${passed}/${results.length} workflows, ` +
    `${results.length * CELLS.length} cells (${document.counts.screen_reader} AT-typed); ${problems.length} failures`,
);
if (problems.length > 0 || passed !== results.length) process.exit(1);
