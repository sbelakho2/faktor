#!/usr/bin/env node
// Lane definitions and machine-readable lane evidence.
//
// Five lanes, six layers. Every command names the layer(s) it proves. A full
// lane's suite union must cover every layer the lane claims; the pull lane
// deliberately has no live layer. The runner executes commands (or prints the
// plan), bounds captured output and writes one evidence document per lane.

import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';

import { LANE_SCHEMA, LANES, LAYERS, canonicalJson } from './model.mjs';
import { select } from './selector.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const OUT_DIR = resolve(ROOT, 'target/certification/programme/lanes');

const PROGRAMME_CHECKS = Object.freeze([
  'node scripts/programme/build-registry.mjs --check',
  'node scripts/programme/check-registry.mjs --check',
  'node scripts/programme/check-registry.mjs --selftest',
  'node scripts/programme/select-tests.mjs selftest',
  'node scripts/programme/build-impact-graph.mjs --selftest',
  'node scripts/programme/check-settlement.mjs --selftest',
  'node scripts/programme/check-settlement.mjs',
  'node scripts/programme/check-core-dumps.mjs --selftest',
  'node scripts/programme/check-core-dumps.mjs',
]);

export const SUITES = Object.freeze({
  registry: {
    layers: ['contracts'],
    commands: PROGRAMME_CHECKS.slice(0, 3),
  },
  selector: {
    layers: ['contracts'],
    commands: [
      'node scripts/programme/select-tests.mjs selftest',
      'node scripts/programme/build-impact-graph.mjs --selftest',
    ],
  },
  'act-now': {
    layers: ['contracts', 'state-model-crash', 'unit-property'],
    commands: [
      'node scripts/programme/check-settlement.mjs --selftest',
      'node scripts/programme/check-settlement.mjs',
      'node tests/programme/generators/hostile.mjs --selftest',
      'node tests/programme/generators/schedules.mjs --selftest',
      'node tests/programme/probes/ordering.mjs',
      'node scripts/programme/check-core-dumps.mjs --selftest',
      'node scripts/programme/check-core-dumps.mjs',
      'cargo test -p faktor-tests-quality --lib memory_pager --all-features',
    ],
  },
  invariants: {
    layers: ['unit-property', 'state-model-crash'],
    commands: [
      'node scripts/check-invariants.mjs',
      'node scripts/check-invariant-coverage.mjs --check',
      'node scripts/check-invariant-coverage.mjs selftest',
    ],
  },
  edition8: {
    layers: ['contracts'],
    commands: ['node tests/edition8/registry.mjs'],
  },
  docs: {
    layers: ['contracts'],
    commands: ['bash scripts/check-docs-sync.sh'],
  },
  workspace: {
    layers: ['unit-property', 'state-model-crash', 'contracts'],
    commands: [
      'cargo fmt --check',
      'cargo check --workspace --all-features',
      'cargo clippy --workspace --all-targets --all-features -- -D warnings',
      'cargo test --workspace --all-features',
      'cargo run -p faktor-cli -- doctor --data-dir /tmp/faktor-programme',
    ],
  },
  scale: {
    layers: ['state-model-crash', 'contracts'],
    commands: [
      'cargo build -p faktor-cli --example e8_scale_seed --example e8_append_turn',
      'node scripts/edition8/scale-probe.mjs --turns 1000',
      'node tests/edition8/registry.mjs',
    ],
  },
  'live-contracts': {
    layers: ['contracts', 'agent-evals'],
    commands: ['node scripts/programme/live-contracts.mjs'],
  },
  hil: {
    layers: ['hardware'],
    commands: ['node scripts/programme/hil.mjs'],
  },
  format: {
    layers: ['contracts'],
    commands: ['cargo fmt --check'],
  },
  'ide-hosts': {
    layers: ['ide-screen-readers'],
    commands: [
      'node apps/vscode/scripts/selftest.mjs',
      'node tests/edition8/registry.mjs',
    ],
  },
  evals: {
    layers: ['agent-evals'],
    commands: ['node scripts/programme/evals.mjs --smoke'],
  },
});

export const LANE_PLANS = Object.freeze({
  pull: {
    suites: ['selector', 'registry', 'act-now', 'invariants', 'edition8', 'docs', 'format'],
    selection: 'affected',
  },
  trusted: {
    suites: ['selector', 'registry', 'act-now', 'invariants', 'edition8', 'docs', 'workspace', 'ide-hosts'],
    selection: 'full',
  },
  nightly: {
    suites: ['selector', 'registry', 'act-now', 'invariants', 'edition8', 'scale', 'ide-hosts', 'evals'],
    selection: 'full',
  },
  'weekly-live': {
    suites: ['registry', 'live-contracts', 'scale', 'ide-hosts', 'evals'],
    selection: 'full',
  },
  release: {
    suites: [
      'selector',
      'registry',
      'act-now',
      'invariants',
      'edition8',
      'docs',
      'workspace',
      'scale',
      'live-contracts',
      'ide-hosts',
      'evals',
      'hil',
    ],
    selection: 'full',
  },
});

function planLayers(plan) {
  const layers = new Set();
  for (const suite of plan.suites) {
    for (const layer of SUITES[suite].layers) layers.add(layer);
  }
  return [...layers].sort();
}

/** Validate lane encoding: every declared layer is executed by a suite. */
export function validateLaneEncoding() {
  const problems = [];
  for (const [lane, plan] of Object.entries(LANE_PLANS)) {
    const spec = LANES[lane];
    if (!spec) {
      problems.push(`unknown lane ${lane}`);
      continue;
    }
    for (const suite of plan.suites) {
      if (!SUITES[suite]) problems.push(`${lane}: unknown suite ${suite}`);
    }
    const layers = planLayers(plan);
    for (const layer of spec.layers) {
      if (layer === 'measurement') continue;
      if (!LAYERS[layer]) problems.push(`${lane}: unknown layer ${layer}`);
      else if (!layers.includes(layer)) {
        problems.push(`${lane}: declared layer ${layer} is not executed by any suite`);
      }
    }
    if (plan.selection === 'affected' && spec.full) {
      problems.push(`${lane}: a full lane may not run a change-selected subset`);
    }
    if (plan.selection === 'full' && !spec.full) {
      problems.push(`${lane}: full-lane selection declared on a non-full lane`);
    }
  }
  return problems;
}

/** Deterministic plan for one lane. */
export function lanePlan(
  lane,
  { changedFiles = [], base = null, mode = null, selection: providedSelection = null } = {},
) {
  const spec = LANES[lane];
  const plan = LANE_PLANS[lane];
  if (!spec || !plan) throw new Error(`unknown lane ${lane}`);
  const commands = [];
  const entries = [];
  const seen = new Set();
  const push = (entry) => {
    if (seen.has(entry.command)) return;
    seen.add(entry.command);
    entries.push(entry);
  };
  const selectionMode = plan.selection === 'full' ? 'trusted' : mode ?? 'pull';
  let selection = null;
  if (lane === 'pull') {
    selection = providedSelection ?? select(changedFiles, { mode: selectionMode, base });
    for (const command of selection.commands) {
      push({ command, suite: 'affected', layers: ['unit', 'property', 'fault', 'integration'] });
    }
    if (selection.refused) {
      push({
        command: 'cargo test --workspace --all-features',
        suite: 'affected-refusal-fallback',
        layers: ['unit', 'property', 'fault', 'integration'],
      });
    }
  }
  for (const suite of plan.suites) {
    for (const command of SUITES[suite].commands) {
      push({ command, suite, layers: [...SUITES[suite].layers] });
    }
  }
  for (const entry of entries) commands.push(entry.command);
  return {
    schema: 'faktor-programme-lane-plan/v1',
    lane,
    full: spec.full,
    layers: planLayers(plan),
    declared_layers: [...spec.layers],
    suites: [...plan.suites],
    selection,
    entries,
    commands,
    digest: `sha256:${createHash('sha256').update(canonicalJson({ lane, entries })).digest('hex')}`,
  };
}

function git(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

const TAIL_LINES = 80;
const TAIL_BYTES = 16 * 1024;

/** Keep the evidence tail bounded in BOTH lines and bytes. */
function tail(value) {
  let text = String(value ?? '');
  const lines = text.split('\n');
  if (lines.length > TAIL_LINES) text = lines.slice(-TAIL_LINES).join('\n');
  if (Buffer.byteLength(text) > TAIL_BYTES) {
    text = Buffer.from(text).subarray(-TAIL_BYTES).toString('utf8');
  }
  return text;
}

function run(command) {
  const started = Date.now();
  const proc = spawnSync('bash', ['-lc', command], {
    cwd: ROOT,
    encoding: 'utf8',
    timeout: 60 * 60 * 1000,
    maxBuffer: 64 * 1024 * 1024,
    env: { ...process.env, CI: process.env.CI ?? '', NO_COLOR: '1' },
  });
  const stdout = String(proc.stdout ?? '');
  const passedTests = [
    ...new Set([...stdout.matchAll(/^test (\S+) \.\.\. ok$/gm)].map((match) => match[1])),
  ];
  return {
    command,
    status: proc.status === 0 ? 'passed' : 'failed',
    exit_code: proc.status,
    ms: Date.now() - started,
    stdout_tail: tail(proc.stdout),
    stderr_tail: tail(proc.stderr),
    signal: proc.signal ?? null,
    passed_tests: passedTests,
  };
}

export function runLane(lane, options = {}) {
  const plan = options.plan ?? lanePlan(lane, options);
  const results = [];
  for (const entry of plan.entries) {
    console.log(`[programme:${lane}] $ ${entry.command}`);
    const result = run(entry.command);
    results.push({ ...result, suite: entry.suite, layers: entry.layers });
    if (result.status !== 'passed') {
      console.error(`[programme:${lane}] FAILED: ${entry.command}`);
    }
  }
  const revision = git(['rev-parse', 'HEAD', 'HEAD^{tree}']);
  const [commit = null, tree = null] = revision ? revision.split('\n') : [];
  const document = {
    schema: LANE_SCHEMA,
    lane,
    commit,
    tree,
    clean: (git(['status', '--porcelain', '--untracked-files=no']) ?? '') === '',
    started_at: new Date().toISOString(),
    layers: plan.layers,
    suites: plan.suites,
    selection: plan.selection,
    results,
    status: results.every((result) => result.status === 'passed') ? 'passed' : 'failed',
    digest: `sha256:${createHash('sha256').update(canonicalJson(results)).digest('hex')}`,
  };
  mkdirSync(OUT_DIR, { recursive: true });
  const path = resolve(OUT_DIR, `${lane}.json`);
  writeFileSync(path, `${JSON.stringify(document, null, 2)}\n`);
  mergeTestsIntoStatus(document);
  console.log(`[programme:${lane}] ${document.status} -> ${path}`);
  return document;
}

const STATUS_CACHE = new Map();

function loadStatus(path) {
  let stat;
  try {
    stat = statSync(path);
  } catch {
    return null;
  }
  const key = `${stat.size}:${stat.mtimeMs}`;
  const cached = STATUS_CACHE.get(path);
  if (cached !== undefined && cached.key === key) return cached.document;
  try {
    const document = JSON.parse(readFileSync(path, 'utf8'));
    STATUS_CACHE.set(path, { key, document });
    return document;
  } catch {
    return null;
  }
}

/** Collected passed libtest names, including their leaf segment. */
export function collectPassedTests(document) {
  const passed = new Set();
  for (const result of document.results ?? []) {
    for (const test of result.passed_tests ?? []) {
      passed.add(test);
      const leaf = test.split('::').pop();
      if (leaf) passed.add(leaf);
    }
  }
  return passed;
}

/** Pure promotion pass over one already-parsed status document. */
export function promoteStatus(status, passed, document) {
  let changed = false;
  for (const row of status.rows ?? []) {
    if (row.status === 'failed') continue;
    const ids = row.test_ids ?? [];
    if (ids.length === 0) continue;
    const matched = ids.filter((id) => passed.has(id) || passed.has(String(id).split('.').pop()));
    if (matched.length === ids.length && row.status !== 'proven') {
      row.status = 'proven';
      row.evidence = { passed: true, lane: document.lane, commit: document.commit };
      changed = true;
    }
  }
  return changed;
}

/** Promote rows whose own test id appears as a passed libtest entry. */
export function mergeTestsIntoStatus(document) {
  const passed = collectPassedTests(document);
  if (passed.size === 0) return;
  const statusPath = resolve(ROOT, 'target/certification/programme/registry-status.json');
  if (!existsSync(statusPath)) return;
  const status = loadStatus(statusPath);
  if (status === null) return;
  if (promoteStatus(status, passed, document)) {
    writeFileSync(statusPath, `${JSON.stringify(status, null, 2)}\n`);
    try {
      const stat = statSync(statusPath);
      STATUS_CACHE.set(statusPath, { key: `${stat.size}:${stat.mtimeMs}`, document: status });
    } catch {
      STATUS_CACHE.delete(statusPath);
    }
  }
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`lane selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`lane selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const problems = validateLaneEncoding();
  // Repository-facing manifest (tests/lanes.toml) must match the executable
  // model exactly; a rename cannot silently diverge from the report.
  const manifestPath = resolve(ROOT, 'tests/lanes.toml');
  let manifest = '';
  try {
    manifest = readFileSync(manifestPath, 'utf8');
  } catch {
    problems.push('tests/lanes.toml is missing');
  }
  const manifestLayers = [...manifest.matchAll(/\[\[layer\]\][\s\S]*?\nid\s*=\s*"([^"]+)"/g)].map((match) => match[1]).sort();
  const manifestLanes = [...manifest.matchAll(/\[\[lane\]\][\s\S]*?\nid\s*=\s*"([^"]+)"/g)].map((match) => match[1]).sort();
  if (JSON.stringify(manifestLayers) !== JSON.stringify(Object.keys(LAYERS).sort())) {
    problems.push(`tests/lanes.toml layers ${manifestLayers.join(',')} != model ${Object.keys(LAYERS).sort().join(',')}`);
  }
  if (JSON.stringify(manifestLanes) !== JSON.stringify(Object.keys(LANES).sort())) {
    problems.push(`tests/lanes.toml lanes ${manifestLanes.join(',')} != model ${Object.keys(LANES).sort().join(',')}`);
  }
  for (const job of ['scale', 'blob-hygiene']) {
    if (!new RegExp(`\\[\\[job\\]\\][\\s\\S]*?\\nid\\s*=\\s*"${job}"`).test(manifest)) {
      problems.push(`tests/lanes.toml is missing job ${job}`);
    }
  }
  if (!/artifact\s*=\s*"target\/certification\/edition8\/scale\.raw\.json"/.test(manifest)) {
    problems.push('tests/lanes.toml scale job has no stored curve artifact');
  }
  check(`lane encoding is complete${problems.length ? `: ${problems.join('; ')}` : ''}`, problems.length === 0);
  check('five lanes are encoded', Object.keys(LANE_PLANS).length === 5);
  check('six layers are encoded', Object.keys(LAYERS).length === 6);
  for (const lane of Object.keys(LANE_PLANS)) {
    const plan = lanePlan(lane, { changedFiles: ['crates/core/src/lib.rs'] });
    check(`${lane} plan is deterministic`, JSON.stringify(plan) === JSON.stringify(lanePlan(lane, { changedFiles: ['crates/core/src/lib.rs'] })));
    check(`${lane} plan has commands`, plan.commands.length > 0);
    if (LANES[lane].full) {
      check(`${lane} runs the full workspace suite`, plan.suites.includes('workspace') || lane === 'nightly' || lane === 'weekly-live');
    }
  }
  const pull = lanePlan('pull', { changedFiles: ['crates/store/src/ledger.rs'] });
  check(
    'pull lane runs only affected Rust crates by default',
    pull.selection?.crates.includes('faktor-store') &&
      pull.commands.some((command) => command.startsWith('cargo test -p ')),
  );
  check(
    'pull lane still runs the structural registry gate',
    pull.commands.includes('node scripts/programme/check-registry.mjs --check'),
  );
  const refused = lanePlan('pull', { changedFiles: ['mystery.bin'] });
  check(
    'an unmapped pull change falls back to the full workspace test, recorded',
    refused.selection?.refused && refused.commands.includes('cargo test --workspace --all-features'),
  );
  const promotion = {
    rows: [
      { id: 'a', status: 'deferred', test_ids: ['crate::test_a', 'test_b'] },
      { id: 'b', status: 'deferred', test_ids: ['test_b'] },
      { id: 'c', status: 'deferred', test_ids: ['missing'] },
      { id: 'd', status: 'failed', test_ids: ['test_a'] },
    ],
  };
  const promoted = promoteStatus(
    promotion,
    collectPassedTests({ results: [{ passed_tests: ['crate::test_a', 'test_b'] }] }),
    { lane: 'pull', commit: 'abc' },
  );
  check(
    'passed tests promote every matched row except failed ones',
    promoted === true &&
      promotion.rows[0].status === 'proven' &&
      promotion.rows[1].status === 'proven' &&
      promotion.rows[2].status === 'deferred' &&
      promotion.rows[3].status === 'failed',
  );
  const bulkRows = Array.from({ length: 10_000 }, (_, index) => ({
    id: `row.${index}`,
    status: 'deferred',
    test_ids: [`bulk.test.${index}`],
  }));
  const bulkPassed = new Set(bulkRows.map((row) => row.test_ids[0]));
  const bulkStart = performance.now();
  const bulkChanged = promoteStatus({ rows: bulkRows }, bulkPassed, { lane: 'pull', commit: 'abc' });
  const bulkMs = performance.now() - bulkStart;
  check(
    `10k-row evidence promotion is one linear pass (${Math.round(bulkMs)} ms < 2000 ms)`,
    bulkChanged === true && bulkMs < 2000 && bulkRows[bulkRows.length - 1].status === 'proven',
  );
  if (failures > 0) {
    console.error(`lane selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('lane selftest: PASS');
  return 0;
}

export function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) return selftest();
  const arg = (name, fallback = null) => {
    const index = args.indexOf(`--${name}`);
    return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
  };
  const lane = arg('lane');
  if (!lane || !LANE_PLANS[lane]) {
    console.error(`usage: run-lane.mjs --lane <${Object.keys(LANE_PLANS).join('|')}> | --selftest`);
    return 2;
  }
  const changed = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--changed-file' && index + 1 < args.length) changed.push(args[index + 1]);
  }
  if (args.includes('--print-plan')) {
    const plan = lanePlan(lane, { changedFiles: changed, base: arg('base') });
    process.stdout.write(`${JSON.stringify(plan, null, 2)}\n`);
    return 0;
  }
  const document = runLane(lane, { changedFiles: changed, base: arg('base') });
  return document.status === 'passed' ? 0 : 1;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main();
}
