#!/usr/bin/env node
// Release gate: all five lanes green at ONE commit, and every registry row
// proven at that commit.
//
//   node scripts/programme/release-gate.mjs --check
//   node scripts/programme/release-gate.mjs --status   (report, never green)
//   node scripts/programme/release-gate.mjs --selftest
//
// Exit codes: 0 release-ready; 1 not ready; 2 usage.

import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { LANE_SCHEMA, LAYERS, canonicalJson } from '../../tests/programme/model.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const LANES = ['pull', 'trusted', 'nightly', 'weekly-live', 'release'];
const OUT = resolve(ROOT, 'target/certification/programme/release.json');
const STATUS = resolve(ROOT, 'target/certification/programme/registry-status.json');

function readJson(path) {
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function evaluate({ registry, lanes }) {
  const problems = [];
  if (registry === null) {
    problems.push('registry status evidence is missing (run check-registry.mjs --release)');
  } else {
    const unproven = (registry.rows ?? []).filter((row) => row.status !== 'proven');
    for (const row of unproven.slice(0, 40)) {
      problems.push(`row not proven: ${row.id} (${row.status})`);
    }
    if (unproven.length > 40) problems.push(`... ${unproven.length - 40} more unproven rows`);
  }
  const commits = new Set();
  const trees = new Set();
  const coveredLayers = new Set();
  for (const lane of LANES) {
    const document = lanes[lane];
    if (!document) {
      problems.push(`lane evidence missing: ${lane}`);
      continue;
    }
    if (document.schema !== LANE_SCHEMA) {
      problems.push(`lane ${lane}: wrong schema ${document.schema}`);
    }
    if (document.status !== 'passed') {
      problems.push(`lane ${lane}: status ${document.status}`);
    }
    if (document.commit) commits.add(document.commit);
    if (document.tree) trees.add(document.tree);
    for (const layer of document.layers ?? []) coveredLayers.add(layer);
  }
  if (commits.size > 1) problems.push(`lanes are not at one commit: ${[...commits].join(', ')}`);
  if (trees.size > 1) problems.push(`lanes are not on one tree: ${[...trees].join(', ')}`);
  if (registry?.commit && commits.size === 1 && !commits.has(registry.commit)) {
    problems.push(`registry evidence commit ${registry.commit} differs from lane commit ${[...commits][0]}`);
  }
  for (const layer of Object.keys(LAYERS)) {
    if (!coveredLayers.has(layer)) problems.push(`release: no passing lane covers layer ${layer}`);
  }
  return { problems, commits: [...commits], trees: [...trees], layers: [...coveredLayers].sort() };
}

function sha(text) {
  return createHash('sha256').update(canonicalJson(text)).digest('hex');
}

function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`release selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`release selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const registry = { commit: 'abc', rows: [{ id: 'x', status: 'proven' }] };
  const layers = ['unit-property', 'state-model-crash', 'contracts', 'ide-screen-readers', 'agent-evals', 'hardware'];
  const lane = (commit = 'abc') => ({
    schema: LANE_SCHEMA,
    status: 'passed',
    commit,
    tree: 'tree',
    layers,
  });
  const good = evaluate({ registry, lanes: Object.fromEntries(LANES.map((id) => [id, lane()])) });
  check('all five lanes at one commit pass', good.problems.length === 0, good.problems.join('; '));
  const missing = evaluate({ registry, lanes: { pull: lane() } });
  check('a missing lane fails', missing.problems.some((problem) => problem.includes('lane evidence missing')));
  const split = evaluate({
    registry,
    lanes: Object.fromEntries(LANES.map((id) => [id, lane(id === 'release' ? 'def' : 'abc')])),
  });
  check('different commits fail', split.problems.some((problem) => problem.includes('not at one commit')));
  const unproven = evaluate({
    registry: { commit: 'abc', rows: [{ id: 'x', status: 'deferred' }] },
    lanes: Object.fromEntries(LANES.map((id) => [id, lane()])),
  });
  check('an unproven row fails release', unproven.problems.some((problem) => problem.includes('row not proven')));
  const failed = evaluate({
    registry,
    lanes: Object.fromEntries(LANES.map((id) => [id, { ...lane(), status: id === 'nightly' ? 'failed' : 'passed' }])),
  });
  check('a failed lane fails release', failed.problems.some((problem) => problem.includes('nightly: status failed')));
  if (failures > 0) {
    console.error(`release selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('release selftest: PASS');
  return 0;
}

export function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) return selftest();
  const registry = readJson(STATUS);
  const lanes = Object.fromEntries(LANES.map((lane) => [lane, readJson(resolve(ROOT, `target/certification/programme/lanes/${lane}.json`))]));
  const evaluation = evaluate({ registry, lanes });
  const document = {
    schema: 'faktor-programme-release-gate/v1',
    at: new Date().toISOString(),
    ready: evaluation.problems.length === 0,
    commits: evaluation.commits,
    trees: evaluation.trees,
    layers: evaluation.layers,
    problems: evaluation.problems,
    digest: `sha256:${sha(evaluation)}`,
  };
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`);
  if (args.includes('--status')) {
    console.log(`release gate: ${document.ready ? 'READY' : 'NOT READY'} (${evaluation.problems.length} problem(s))`);
    for (const problem of evaluation.problems.slice(0, 12)) console.log(`  ${problem}`);
    return 0;
  }
  if (!document.ready) {
    console.error(`release gate: NOT READY (${evaluation.problems.length} problem(s))`);
    for (const problem of evaluation.problems.slice(0, 20)) console.error(`  ${problem}`);
    return 1;
  }
  console.log(`release gate: READY (${evaluation.commits[0]})`);
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main();
}
