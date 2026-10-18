#!/usr/bin/env node
// Edition 5 gate runner. Runs every registry-driven gate, converts the raw
// host outputs into `faktor-edition5-gate/v1` evidence, and writes the
// certificate summary consumed by the Woodpecker certificate step.
//
// Usage:
//   node scripts/edition5/run.mjs                 # all gates
//   node scripts/edition5/run.mjs --skip-jetbrains
//   node scripts/edition5/run.mjs --dogfood       # also run the isolated dogfood
//   node scripts/edition5/run.mjs --summary       # only re-write the summary
//
// Every gate is independently executable; this runner adds no hidden state.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry, summarizeRegistry, validateRegistry } from '../../tests/edition5/registry.mjs';
import {
  GATES,
  writeGate,
  writeSummary,
  verifyGates,
} from '../../tests/edition5/evidence.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition5');
const argv = process.argv.slice(2);
const skipJetbrains = argv.includes('--skip-jetbrains');
const buildJetbrains = argv.includes('--build-jetbrains');
const noVerify = argv.includes('--no-verify');
const runDogfood = argv.includes('--dogfood');
const summaryOnly = argv.includes('--summary');

function readRaw(name) {
  const path = resolve(OUT, name);
  if (!existsSync(path)) {
    throw new Error(`missing raw evidence ${name}; run the ${name.split('.')[0]} gate first`);
  }
  return JSON.parse(readFileSync(path, 'utf8'));
}

function run(script, args = []) {
  console.log(`[edition5] ${script} ${args.join(' ')}`.trim());
  execFileSync(process.execPath, [resolve(ROOT, script), ...args], {
    cwd: ROOT,
    stdio: 'inherit',
  });
}

function runShell(script, args = []) {
  console.log(`[edition5] ${script} ${args.join(' ')}`.trim());
  execFileSync('bash', [resolve(ROOT, script), ...args], {
    cwd: ROOT,
    stdio: 'inherit',
    env: { ...process.env },
  });
}

function requireCounts(document, expected, label) {
  for (const [key, value] of Object.entries(expected)) {
    if (document.counts?.[key] !== value) {
      throw new Error(`${label}: counts.${key} is ${document.counts?.[key]} (expected ${value})`);
    }
  }
}

function convert() {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) throw new Error(problems[0]);

  const vscodeControls = readRaw('control-exercise-vscode.raw.json');
  requireCounts(vscodeControls, { controls: 200, exercised: 200, failed: 0, missing_handlers: 0 }, 'vscode control exercise');
  writeGate('control-exercise-vscode', {
    status: 'passed',
    counts: vscodeControls.counts,
    results: vscodeControls.results,
    artifacts: { raw: 'control-exercise-vscode.raw.json', screens: vscodeControls.screens ?? null },
  });

  if (!skipJetbrains) {
    const jetControls = readRaw('control-exercise-jetbrains.raw.json');
    requireCounts(jetControls, { controls: 200, exercised: 200, failed: 0, missing_handlers: 0 }, 'jetbrains control exercise');
    writeGate('control-exercise-jetbrains', {
      status: 'passed',
      counts: jetControls.counts,
      results: jetControls.results,
      artifacts: { raw: 'control-exercise-jetbrains.raw.json' },
    });
  }

  const vscodeAcceptance = readRaw('acceptance-vscode.raw.json');
  requireCounts(vscodeAcceptance, { scenarios: 40, passed: 40, failed: 0 }, 'vscode acceptance');
  writeGate('acceptance-vscode', {
    status: 'passed',
    counts: vscodeAcceptance.counts,
    results: vscodeAcceptance.results,
    artifacts: { raw: 'acceptance-vscode.raw.json' },
  });

  if (!skipJetbrains) {
    const jetAcceptance = readRaw('acceptance-jetbrains.raw.json');
    requireCounts(jetAcceptance, { scenarios: 40, passed: 40, failed: 0 }, 'jetbrains acceptance');
    writeGate('acceptance-jetbrains', {
      status: 'passed',
      counts: jetAcceptance.counts,
      results: jetAcceptance.results,
      artifacts: { raw: 'acceptance-jetbrains.raw.json' },
    });
  }

  const visual = readRaw('visual-contracts.raw.json');
  requireCounts(visual, { screens: 32, hosts: 2, contracts: 64, failed: 0 }, 'visual contracts');
  writeGate('visual-contracts', {
    status: 'passed',
    counts: visual.counts,
    results: visual.results,
    artifacts: { raw: 'visual-contracts.raw.json', contracts: 'visual-contracts.raw.json' },
    detail: { platform_status: visual.platform_status },
  });

  const settings = readRaw('settings-contracts.raw.json');
  requireCounts(settings, { failed: 0 }, 'settings contracts');
  writeGate('settings-contracts', {
    status: 'passed',
    counts: settings.counts,
    results: settings.results,
    artifacts: { raw: 'settings-contracts.raw.json' },
  });

  const perf = readRaw('perf-delta.raw.json');
  requireCounts(perf, { workloads: 4, failed: 0 }, 'perf delta');
  writeGate('perf-delta', {
    status: 'passed',
    counts: { ...perf.counts, workloads: 4 },
    results: perf.results,
    artifacts: { raw: 'perf-delta.raw.json' },
  });

  const dead = readRaw('dead-controls.raw.json');
  requireCounts(dead, { dead: 0, missing_handlers: 0, orphan_handlers: 0 }, 'dead controls');
  writeGate('dead-controls', {
    status: 'passed',
    counts: dead.counts,
    results: dead.results,
    artifacts: { raw: 'dead-controls.raw.json' },
  });

  if (existsSync(resolve(OUT, 'dogfood.raw.json'))) {
    const dogfood = readRaw('dogfood.raw.json');
    const hostPath = resolve(OUT, 'dogfood-vscode.raw.json');
    const host = existsSync(hostPath) ? JSON.parse(readFileSync(hostPath, 'utf8')) : null;
    const rows = [...(dogfood.results ?? [])];
    if (host !== null) {
      for (const row of host.results ?? []) {
        rows.push({
          id: row.id,
          status: row.status,
          passed: row.passed,
          detail: `vscode-host: ${row.detail}`,
        });
      }
    }
    const required = [
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
    const ids = new Set(rows.filter((row) => row.passed === true).map((row) => row.id));
    const missing = required.filter((id) => !ids.has(id));
    if (missing.length > 0) {
      throw new Error(`dogfood: missing passed flows ${missing.join(', ')}`);
    }
    writeGate('dogfood', {
      status: 'passed',
      counts: {
        flows: rows.length,
        passed: rows.filter((row) => row.passed === true).length,
        failed: rows.filter((row) => row.passed !== true).length,
        network: 'loopback-only',
        isolated: true,
      },
      results: rows,
      artifacts: {
        raw: 'dogfood.raw.json',
        vscode: 'dogfood-vscode.raw.json',
      },
    });
  }
}

function main() {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`edition5 registry: ${problem}`);
    process.exit(1);
  }
  console.log(
    `[edition5] registry ${summarizeRegistry(registry).controls} controls / ` +
      `${registry.screens.length} screens / ${registry.acceptance.length} scenarios`,
  );
  if (!summaryOnly) {
    run('tests/edition5/registry.mjs');
    run('apps/vscode/scripts/edition5-control-exercise.mjs');
    run('apps/vscode/scripts/edition5-acceptance.mjs');
    if (!skipJetbrains) {
      const haveControlRaw = existsSync(resolve(OUT, 'control-exercise-jetbrains.raw.json'));
      const haveOverlays = existsSync(resolve(OUT, 'acceptance-jetbrains.overlays.json'));
      if (buildJetbrains || !haveControlRaw || !haveOverlays) {
        runShell('apps/jetbrains/compile-and-smoke.sh', ['--smoke', 'ControlExercise', '--smoke', 'Acceptance']);
      } else {
        console.log('[edition5] using JetBrains control/overlay evidence from the pipeline workspace');
      }
      // The Node runner is cheap and always converts the host overlays into
      // the shared A01-A40 result document.
      run('scripts/edition5/acceptance-jetbrains.mjs');
    }
    run('tests/edition5/visual-contracts.mjs');
    run('tests/edition5/settings-contract.mjs');
    run('tests/edition5/perf-delta.mjs');
    run('tests/edition5/dead-control.mjs');
    if (runDogfood) {
      runShell('scripts/edition5/dogfood.sh');
    }
    convert();
  }
  mkdirSync(OUT, { recursive: true });
  const summary = writeSummary();
  console.log(`[edition5] summary written: ${resolve(ROOT, 'target/certification/edition5-summary.json')}`);
  console.log(JSON.stringify(summary.controls));
  if (!noVerify && !skipJetbrains && existsSync(resolve(OUT, 'dogfood.raw.json'))) {
    const verifyProblems = verifyGates({ allowDirty: true });
    if (verifyProblems.length > 0) {
      for (const problem of verifyProblems) console.error(`edition5 evidence: ${problem}`);
      process.exit(1);
    }
    console.log('[edition5] all gates present, current and counted');
  } else {
    console.log('[edition5] partial run: certificate verification skipped (host/dogfood gates not selected)');
  }
}

main();
