#!/usr/bin/env node
// Machine-readable evidence for every Edition 5 gate. Each gate writes one
// `faktor-edition5-gate/v1` document into target/certification/edition5/;
// `scripts/edition5/evidence.mjs verify` (and the Woodpecker certificate
// step) consumes them all and refuses a missing, stale or under-counted gate.
//
// Evidence is bound to the exact HEAD commit/tree and to the registry inputs
// digest. Counts are never inferred from prose: callers pass the actual
// exercised/failed numbers, and `verify` re-derives the required totals from
// controls.json / screens.md / flows.md / acceptance.md.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  EXPECTED,
  buildRegistry,
  summarizeRegistry,
  validateRegistry,
} from './registry.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const OUT_DIR = resolve(ROOT, 'target/certification/edition5');
export const SUMMARY_PATH = resolve(ROOT, 'target/certification/edition5-summary.json');
export const GATE_SCHEMA = 'faktor-edition5-gate/v1';
export const SUMMARY_SCHEMA = 'faktor-edition5-certificate/v1';

export const GATES = Object.freeze({
  'control-exercise-vscode': { kind: 'control-exercise', host: 'vscode' },
  'control-exercise-jetbrains': { kind: 'control-exercise', host: 'jetbrains' },
  'acceptance-vscode': { kind: 'acceptance', host: 'vscode' },
  'acceptance-jetbrains': { kind: 'acceptance', host: 'jetbrains' },
  'visual-contracts': { kind: 'visual-contracts' },
  'settings-contracts': { kind: 'settings-contracts' },
  'perf-delta': { kind: 'perf-delta' },
  'dead-controls': { kind: 'dead-controls' },
  'dogfood': { kind: 'dogfood' },
});

function git(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

export function headIdentity() {
  return {
    commit: git(['rev-parse', 'HEAD']),
    tree: git(['rev-parse', 'HEAD^{tree}']),
    dirty: (git(['status', '--porcelain', '--untracked-files=no']) ?? '').length > 0,
  };
}

export function nowIso() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

export function writeGate(name, payload) {
  const spec = GATES[name];
  if (!spec) {
    throw new Error(`unregistered edition5 gate ${name}`);
  }
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    throw new Error(`edition5 registry invalid: ${problems[0]}`);
  }
  const identity = headIdentity();
  const document = {
    schema: GATE_SCHEMA,
    gate: name,
    kind: spec.kind,
    host: spec.host ?? 'both',
    registry: summarizeRegistry(registry),
    commit: identity.commit,
    tree: identity.tree,
    dirty: identity.dirty,
    generated_at: nowIso(),
    status: payload.status ?? 'passed',
    counts: payload.counts ?? {},
    results: payload.results ?? [],
    artifacts: payload.artifacts ?? {},
    detail: payload.detail ?? null,
  };
  mkdirSync(OUT_DIR, { recursive: true });
  const path = resolve(OUT_DIR, `${name}.json`);
  writeFileSync(path, `${JSON.stringify(document, null, 2)}\n`);
  return document;
}

export function readGate(name) {
  const path = resolve(OUT_DIR, `${name}.json`);
  if (!existsSync(path)) {
    return null;
  }
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function evidenceFiles() {
  if (!existsSync(OUT_DIR)) {
    return [];
  }
  return readdirSync(OUT_DIR)
    .filter((name) => name.endsWith('.json'))
    .sort();
}

/**
 * Verify every required gate for the current commit/tree. Returns a list of
 * problems. The check is deliberately strict about counts:
 *   control exercise: 200 controls per host, no failures/missing handlers
 *   acceptance:       40 scenarios per host, no failures
 *   visual:           32 screens x 2 hosts, with per-platform records
 *   dogfood:          all required flows passed
 */
export function verifyGates({ allowDirty = false } = {}) {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    return problems;
  }
  const identity = headIdentity();
  const required = Object.keys(GATES);
  const docs = {};
  for (const name of required) {
    const doc = readGate(name);
    if (doc === null) {
      problems.push(`missing evidence: ${name}`);
      continue;
    }
    docs[name] = doc;
    if (doc.schema !== GATE_SCHEMA) {
      problems.push(`${name}: wrong schema ${String(doc.schema)}`);
    }
    if (doc.commit !== identity.commit) {
      problems.push(`${name}: bound to another commit`);
    }
    if (doc.tree !== identity.tree) {
      problems.push(`${name}: bound to another tree`);
    }
    if (doc.registry.inputs_digest !== summarizeRegistry(registry).inputs_digest) {
      problems.push(`${name}: registry inputs changed after evidence was written`);
    }
    if (doc.status !== 'passed') {
      problems.push(`${name}: status ${doc.status}`);
    }
    if (doc.dirty && !allowDirty) {
      problems.push(`${name}: recorded a dirty checkout (re-run the gate on the final tree)`);
    }
  }

  const requireCount = (doc, key, expected, label) => {
    const actual = doc.counts?.[key];
    if (actual !== expected) {
      problems.push(`${doc.gate}: ${label} ${String(actual)} != ${expected}`);
    }
  };

  for (const host of ['vscode', 'jetbrains']) {
    const controls = docs[`control-exercise-${host}`];
    if (controls) {
      requireCount(controls, 'controls', EXPECTED.controls, 'controls');
      requireCount(controls, 'exercised', EXPECTED.controls, 'exercised');
      requireCount(controls, 'failed', 0, 'failed');
      requireCount(controls, 'missing_handlers', 0, 'missing handlers');
      if (controls.counts?.effective + controls.counts?.unavailable !== EXPECTED.controls) {
        problems.push(`${host}: effective + unavailable must equal ${EXPECTED.controls}`);
      }
      if (!Array.isArray(controls.results) || controls.results.length !== EXPECTED.controls) {
        problems.push(`${host}: per-control results must cover all ${EXPECTED.controls} rows`);
      } else {
        const ids = new Set(controls.results.map((row) => row.id));
        if (ids.size !== EXPECTED.controls) {
          problems.push(`${host}: duplicate/missing control result ids`);
        }
        for (const row of controls.results) {
          if (!row.observable || typeof row.observable !== 'string') {
            problems.push(`${host}: ${row.id} has no observable effect`);
          }
          if (!row.handler) {
            problems.push(`${host}: ${row.id} has no handler`);
          }
        }
      }
    }
    const acceptance = docs[`acceptance-${host}`];
    if (acceptance) {
      requireCount(acceptance, 'scenarios', EXPECTED.acceptance, 'scenarios');
      requireCount(acceptance, 'passed', EXPECTED.acceptance, 'passed');
      requireCount(acceptance, 'failed', 0, 'failed');
      if (!Array.isArray(acceptance.results) || acceptance.results.length !== EXPECTED.acceptance) {
        problems.push(`${host}: acceptance results must cover A01-A40`);
      } else {
        const ids = new Set(acceptance.results.map((row) => row.id));
        for (let index = 1; index <= EXPECTED.acceptance; index += 1) {
          const id = `A${String(index).padStart(2, '0')}`;
          if (!ids.has(id)) {
            problems.push(`${host}: missing acceptance result ${id}`);
          }
        }
      }
    }
  }

  const visual = docs['visual-contracts'];
  if (visual) {
    requireCount(visual, 'screens', EXPECTED.screens, 'screens');
    requireCount(visual, 'hosts', 2, 'hosts');
    requireCount(visual, 'contracts', EXPECTED.screens * 2, 'contracts');
    requireCount(visual, 'failed', 0, 'failed');
    const platforms = visual.counts?.platforms ?? [];
    for (const platform of ['linux', 'macos', 'windows']) {
      if (!platforms.includes(platform)) {
        problems.push(`visual-contracts: missing platform ${platform}`);
      }
    }
  }

  const settings = docs['settings-contracts'];
  if (settings) {
    requireCount(settings, 'failed', 0, 'failed');
    if (!Array.isArray(settings.results) || settings.results.length === 0) {
      problems.push('settings-contracts: no contract results');
    }
  }

  const perf = docs['perf-delta'];
  if (perf) {
    requireCount(perf, 'failed', 0, 'failed');
    requireCount(perf, 'workloads', 4, 'workloads');
  }

  const dead = docs['dead-controls'];
  if (dead) {
    requireCount(dead, 'dead', 0, 'dead controls');
    requireCount(dead, 'missing_handlers', 0, 'missing handlers');
    requireCount(dead, 'orphan_handlers', 0, 'orphan handlers');
  }

  const dogfood = docs.dogfood;
  if (dogfood) {
    requireCount(dogfood, 'failed', 0, 'failed');
    const requiredFlows = [
      'task-admission',
      'provider-config',
      'compaction-long-task',
      'restore-fork',
      'permissions',
      'proof',
      'finish-levels',
      'vscode-host',
      'jetbrains-host',
    ];
    const flowed = new Set((dogfood.results ?? []).map((row) => row.id));
    for (const flow of requiredFlows) {
      if (!flowed.has(flow)) {
        problems.push(`dogfood: missing flow ${flow}`);
      }
    }
  }

  if (existsSync(SUMMARY_PATH)) {
    try {
      const summary = JSON.parse(readFileSync(SUMMARY_PATH, 'utf8'));
      if (summary.schema !== SUMMARY_SCHEMA) {
        problems.push('edition5 summary: wrong schema');
      }
      if (summary.commit !== identity.commit || summary.tree !== identity.tree) {
        problems.push('edition5 summary: bound to another commit/tree');
      }
    } catch (error) {
      problems.push(`edition5 summary: unreadable (${error && error.message})`);
    }
  } else {
    problems.push('missing edition5 summary');
  }
  return problems;
}

export function buildSummary() {
  const registry = buildRegistry();
  const docs = Object.fromEntries(
    Object.keys(GATES).map((name) => [name, readGate(name)]),
  );
  const identity = headIdentity();
  const host = (name, key) => docs[name]?.counts?.[key] ?? null;
  return {
    schema: SUMMARY_SCHEMA,
    commit: identity.commit,
    tree: identity.tree,
    dirty: identity.dirty,
    generated_at: nowIso(),
    registry: summarizeRegistry(registry),
    gates: Object.fromEntries(
      Object.entries(docs).map(([name, doc]) => [
        name,
        doc === null ? { present: false } : { present: true, status: doc.status, counts: doc.counts },
      ]),
    ),
    controls: {
      vscode: {
        exercised: host('control-exercise-vscode', 'exercised') ?? 0,
        effective: host('control-exercise-vscode', 'effective') ?? 0,
        unavailable: host('control-exercise-vscode', 'unavailable') ?? 0,
        failed: host('control-exercise-vscode', 'failed') ?? 0,
      },
      jetbrains: {
        exercised: host('control-exercise-jetbrains', 'exercised') ?? 0,
        effective: host('control-exercise-jetbrains', 'effective') ?? 0,
        unavailable: host('control-exercise-jetbrains', 'unavailable') ?? 0,
        failed: host('control-exercise-jetbrains', 'failed') ?? 0,
      },
    },
    acceptance: {
      vscode: docs['acceptance-vscode']?.counts ?? null,
      jetbrains: docs['acceptance-jetbrains']?.counts ?? null,
    },
    visual: docs['visual-contracts']?.counts ?? null,
    dogfood: docs.dogfood?.counts ?? null,
    performance: docs['perf-delta']?.counts ?? null,
    dead_controls: docs['dead-controls']?.counts ?? null,
  };
}

export function writeSummary() {
  const summary = buildSummary();
  mkdirSync(dirname(SUMMARY_PATH), { recursive: true });
  writeFileSync(SUMMARY_PATH, `${JSON.stringify(summary, null, 2)}\n`);
  return summary;
}

export function main(argv = process.argv.slice(2)) {
  const [command] = argv;
  if (command === 'summary') {
    const summary = writeSummary();
    console.log(`edition5 evidence summary: ${SUMMARY_PATH}`);
    console.log(JSON.stringify(summary.controls));
    return;
  }
  if (command === 'verify') {
    const problems = verifyGates({ allowDirty: argv.includes('--allow-dirty') });
    if (problems.length > 0) {
      for (const problem of problems) {
        console.error(`edition5 evidence: ${problem}`);
      }
      process.exit(1);
    }
    console.log('edition5 evidence: all gates present, current and counted');
    return;
  }
  if (command === 'list') {
    console.log(JSON.stringify(evidenceFiles(), null, 2));
    return;
  }
  console.error('usage: evidence.mjs <summary|verify|list> [--allow-dirty]');
  process.exit(2);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
