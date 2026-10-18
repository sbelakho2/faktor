#!/usr/bin/env node
// Shared W01-W16 workflow contract. It reads the printed flow names, screen
// paths and completion conditions from the E8 registry, requires five cells
// per host (pointer, keyboard, screen reader, compact/zoom, recovery), and
// validates the host observation document. A cell is never blank: it is either
// passed with a receipt, or an explicit typed boundary whose reason is named.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry } from './registry.mjs';

export const CELLS = ['pointer', 'keyboard', 'screen_reader', 'compact_zoom', 'recovery'];

export const CELL_LABELS = {
  pointer: 'Pointer run',
  keyboard: 'Keyboard-only run',
  screen_reader: 'Screen-reader cell',
  compact_zoom: '240/320 px + 200% text',
  recovery: 'Printed recovery action',
};

const FORBIDDEN = [
  /\bfake\b/i,
  /optimistic completion/i,
  /secret value/i,
  /invented (price|score|usage)/i,
];

const HERE = dirname(fileURLToPath(import.meta.url));

export function workflowFixtures() {
  const fixtures = [];
  for (let index = 1; index <= 16; index += 1) {
    const id = `W${String(index).padStart(2, '0')}`;
    const path = resolve(HERE, 'fixtures', `${id.toLowerCase()}.json`);
    if (!existsSync(path)) continue;
    fixtures.push(JSON.parse(readFileSync(path, 'utf8')));
  }
  return fixtures;
}

export function validateFixtures() {
  const contracts = workflowContracts();
  const fixtures = workflowFixtures();
  const problems = [];
  if (fixtures.length !== contracts.length) problems.push(`expected ${contracts.length} workflow fixtures, got ${fixtures.length}`);
  const byId = new Map(fixtures.map((fixture) => [fixture.workflow, fixture]));
  for (const contract of contracts) {
    const fixture = byId.get(contract.id);
    if (!fixture) {
      problems.push(`missing fixture for ${contract.id}`);
      continue;
    }
    if (fixture.schema !== 'faktor-edition8-workflow-fixture/v1') problems.push(`${contract.id}: wrong fixture schema`);
    if (JSON.stringify(fixture.screens) !== JSON.stringify(contract.screens)) {
      problems.push(`${contract.id}: fixture screens ${JSON.stringify(fixture.screens)} != registry ${JSON.stringify(contract.screens)}`);
    }
    if (JSON.stringify(fixture.cells) !== JSON.stringify(CELLS)) problems.push(`${contract.id}: fixture cells are not the five printed cells`);
    if (!Array.isArray(fixture.steps) || fixture.steps.length === 0) problems.push(`${contract.id}: fixture has no printed steps`);
    if (!Array.isArray(fixture.forbidden_fake_states) || fixture.forbidden_fake_states.length === 0) {
      problems.push(`${contract.id}: fixture has no printed forbidden fake states`);
    }
    if (typeof fixture.complete_when !== 'string' || fixture.complete_when.length < 20) {
      problems.push(`${contract.id}: fixture has no printed completion condition`);
    }
  }
  for (const fixture of fixtures) {
    if (!contracts.some((contract) => contract.id === fixture.workflow)) problems.push(`stale fixture ${fixture.workflow}`);
  }
  return problems;
}

export function workflowContracts() {
  const registry = buildRegistry();
  return registry.workflows.map((workflow) => ({
    id: workflow.id,
    title: workflow.title,
    screens: workflow.screens,
    controls: workflow.controls,
    complete_when: workflow.complete_when,
    forbidden_fake_states: workflow.forbidden_fake_states,
    cells: CELLS.map((id) => ({ id, label: CELL_LABELS[id] })),
  }));
}

function scanForbidden(value, path, problems) {
  if (typeof value === 'string') {
    for (const rule of FORBIDDEN) {
      if (rule.test(value)) problems.push(`${path}: forbidden fake-state text: ${value.slice(0, 140)}`);
    }
  } else if (Array.isArray(value)) {
    value.forEach((entry, index) => scanForbidden(entry, `${path}[${index}]`, problems));
  } else if (value !== null && typeof value === 'object') {
    for (const [key, entry] of Object.entries(value)) scanForbidden(entry, `${path}.${key}`, problems);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const problems = validateFixtures();
  for (const problem of problems) console.error(`edition8 workflows: ${problem}`);
  if (problems.length > 0) process.exit(1);
  console.log(`edition8 workflows: ${workflowFixtures().length} printed fixtures (W01-W16, five cells each)`);
}

export function validateHostWorkflows(document, host) {
  const contracts = workflowContracts();
  const problems = [];
  if (document.schema !== 'faktor-edition8-workflows/v1') problems.push(`${host}: wrong schema`);
  if (document.host !== host) problems.push(`${host}: wrong host stamp`);
  if (!Array.isArray(document.results) || document.results.length !== contracts.length) {
    problems.push(`${host}: expected ${contracts.length} workflow results`);
    return problems;
  }
  const byId = new Map(document.results.map((row) => [row.id, row]));
  for (const contract of contracts) {
    const row = byId.get(contract.id);
    if (!row) {
      problems.push(`${host}: missing workflow ${contract.id}`);
      continue;
    }
    if (row.screens?.join(',') !== contract.screens.join(',')) {
      problems.push(`${host}: ${contract.id} screen path drifted`);
    }
    for (const cell of CELLS) {
      const observed = row.cells?.[cell];
      if (!observed) {
        problems.push(`${host}: ${contract.id} ${cell} cell is blank`);
        continue;
      }
      if (observed.status === 'passed') {
        if (!observed.receipt || observed.receipt.length === 0) problems.push(`${host}: ${contract.id} ${cell} lacks a receipt`);
        if (observed.boundary?.status === 'not_certified' && !observed.boundary.reason) {
          problems.push(`${host}: ${contract.id} ${cell} has an unnamed AT boundary`);
        }
      } else if (observed.status === 'not_certified') {
        if (!observed.reason || observed.reason.length === 0) {
          problems.push(`${host}: ${contract.id} ${cell} has no typed reason`);
        }
      } else {
        problems.push(`${host}: ${contract.id} ${cell} status ${observed.status}`);
      }
    }
    if (row.complete_when !== true) problems.push(`${host}: ${contract.id} did not reach complete-when`);
  }
  scanForbidden(document, host, problems);
  return problems;
}

export function summarizeWorkflows(document, host) {
  const rows = document.results ?? [];
  const failed = rows.filter((row) => row.status !== 'passed');
  const cells = rows.flatMap((row) => CELLS.map((cell) => row.cells?.[cell]).filter(Boolean));
  return {
    workflows: rows.length,
    passed: rows.length - failed.length,
    failed: failed.length,
    cells: cells.length,
    pointer: cells.filter((cell) => cell.mode === 'pointer').length,
    keyboard: cells.filter((cell) => cell.mode === 'keyboard').length,
    screen_reader: cells.filter((cell) => cell.mode === 'aria_automated' || cell.mode === 'at').length,
    compact_zoom: cells.filter((cell) => cell.mode === 'layout').length,
    recovery: cells.filter((cell) => cell.mode === 'recovery').length,
    host,
  };
}

export function runWorkflowContract(observations, host) {
  const contracts = workflowContracts();
  const results = [];
  for (const contract of contracts) {
    const observed = observations[contract.id];
    if (!observed) {
      results.push({ ...contract, host, status: 'failed', cells: {}, detail: 'no host observation', complete_when: false });
      continue;
    }
    const cells = observed.cells ?? {};
    const cellProblems = [];
    for (const cell of CELLS) {
      const value = cells[cell];
      if (!value) {
        cellProblems.push(`${cell}: blank`);
      } else if (value.status !== 'passed' && value.status !== 'not_certified') {
        cellProblems.push(`${cell}: ${value.status}`);
      }
    }
    const complete = observed.complete_when === true;
    if (!complete) cellProblems.push('complete-when not established');
    results.push({
      id: contract.id,
      title: contract.title,
      host,
      screens: contract.screens,
      controls: contract.controls,
      cells,
      complete_when: complete,
      status: cellProblems.length === 0 ? 'passed' : 'failed',
      detail: cellProblems.length === 0 ? null : cellProblems.join('; '),
    });
  }
  return results;
}
