#!/usr/bin/env node
// Change-aware test selection.
//
// Maps changed files (git diff vs a base commit, or an explicit file list) to
// affected crates, test crates and suites. The mapping is CONSERVATIVE: a
// changed crate selects the transitive closure of its reverse workspace
// dependencies, because a change in `faktor-store` can break `faktor-agent`,
// `faktor-server` and every test crate that depends on them.
//
// A path outside the mapped roots is a typed refusal, never a silent skip:
// callers either run the full relevant layer (documented) or fail.
//
// Modes:
//   --mode pull       affected suites only (fast; default)
//   --mode trusted    every workspace crate and suite (full lane)
//   --mode nightly    every workspace crate and suite (full lane)
//   --mode weekly-live every workspace crate and live suites (full lane)
//   --mode release    everything (full lane)

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { canonicalJson } from './model.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

export const FULL_MODES = new Set(['trusted', 'nightly', 'weekly-live', 'release']);

// Heavy test crates whose proofs run in a scheduled/trusted lane. A pull
// selection records them as deferred with a reason; it never silently drops
// them (release re-runs every row).
const PULL_DEFERRED_CRATES = Object.freeze({
  'faktor-tests-longrun': 'nightly longrun lane',
  'faktor-tests-acp-official': 'nightly/trusted ACP corpus lane',
});

function read(path) {
  return readFileSync(path, 'utf8');
}

/** Workspace member directories, from the root manifest (globs expanded). */
export function workspaceMembers(root = ROOT) {
  const text = read(join(root, 'Cargo.toml'));
  const section = /\[workspace\][\s\S]*?members\s*=\s*\[([\s\S]*?)\]/.exec(text);
  if (!section) return [];
  const members = [];
  for (const match of section[1].matchAll(/"([^"]+)"/g)) {
    const pattern = match[1];
    if (!pattern.includes('*')) {
      members.push(pattern);
      continue;
    }
    const base = pattern.slice(0, pattern.indexOf('*')).replace(/\/$/, '');
    const dir = join(root, base);
    if (!existsSync(dir)) continue;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (entry.isDirectory()) members.push(`${base}/${entry.name}`);
    }
  }
  return [...new Set(members)].sort();
}

/**
 * Crate metadata: name, directory, direct workspace dependencies and the
 * test/binary targets that `cargo test -p <name>` compiles.
 */
export function workspaceGraph(root = ROOT) {
  const crates = new Map();
  const byDir = [];
  for (const member of workspaceMembers(root)) {
    const manifest = join(root, member, 'Cargo.toml');
    if (!existsSync(manifest)) continue;
    const text = read(manifest);
    const name = /\[package\][\s\S]*?^name\s*=\s*"([^"]+)"/m.exec(text)?.[1];
    if (!name) continue;
    const deps = new Set();
    let section = '';
    for (const line of text.split('\n')) {
      const header = /^\s*\[([^\]]+)\]/.exec(line);
      if (header) {
        section = header[1];
        continue;
      }
      if (
        !/^(dependencies|dev-dependencies|build-dependencies)$/.test(section) &&
        !/\.(dependencies|dev-dependencies|build-dependencies)$/.test(section)
      ) {
        continue;
      }
      const dep = /^\s*([A-Za-z0-9_-]+)\s*(?:=|\.workspace)/.exec(line);
      if (dep && dep[1].startsWith('faktor-')) deps.add(dep[1]);
    }
    const crate = { name, dir: member, deps: [...deps].sort() };
    crates.set(name, crate);
    byDir.push(crate);
  }
  // Reverse dependency closure over workspace crates.
  const dependents = new Map();
  for (const crate of crates.values()) dependents.set(crate.name, new Set());
  for (const crate of crates.values()) {
    for (const dep of crate.deps) {
      if (dependents.has(dep)) dependents.get(dep).add(crate.name);
    }
  }
  return { crates, byDir, dependents };
}

/** Crate owning a repo-relative path (longest member directory prefix). */
export function crateForPath(path, graph) {
  const normalized = path.split('\\').join('/');
  let best = null;
  for (const crate of graph.byDir) {
    if (normalized === crate.dir || normalized.startsWith(`${crate.dir}/`)) {
      if (best === null || crate.dir.length > best.dir.length) best = crate;
    }
  }
  return best;
}

/** Transitive reverse dependents, including the crate itself. */
export function reverseClosure(names, graph) {
  const selected = new Set();
  const queue = [...names];
  while (queue.length > 0) {
    const name = queue.pop();
    if (selected.has(name)) continue;
    selected.add(name);
    for (const dependent of graph.dependents.get(name) ?? []) {
      if (!selected.has(dependent)) queue.push(dependent);
    }
  }
  return [...selected].filter((name) => graph.crates.has(name)).sort();
}

const SUITE_PATHS = Object.freeze([
  ['tests/edition8/', 'edition8'],
  ['scripts/edition8/', 'edition8'],
  ['docs/specs/edition8/', 'edition8'],
  ['tests/programme/', 'programme'],
  ['scripts/programme/', 'programme'],
  ['apps/vscode/', 'vscode'],
  ['apps/jetbrains/', 'jetbrains'],
  ['apps/', 'vscode'],
  ['.woodpecker/', 'ci'],
  ['docker/', 'ci'],
  ['fuzz/', 'fuzz'],
  ['evidence-cas/', 'contracts'],
  ['scripts/certification/', 'certification'],
  ['scripts/woodpecker/', 'ci'],
  ['docs/contracts/', 'contracts'],
  ['fixtures/', 'fixtures'],
  ['ui/', 'branding'],
  ['scripts/', 'scripts'],
  ['docs/', 'docs'],
]);

const SUITE_COMMANDS = Object.freeze({
  programme: [
    'node scripts/programme/build-registry.mjs --check',
    'node scripts/programme/check-registry.mjs --check',
    'node scripts/programme/check-registry.mjs --selftest',
    'node scripts/programme/select-tests.mjs selftest',
    'node scripts/programme/run-lane.mjs --selftest',
    'node scripts/programme/check-core-dumps.mjs --selftest',
    'node scripts/programme/check-settlement.mjs --selftest',
  ],
  edition8: ['node tests/edition8/registry.mjs'],
  vscode: [
    'npm --prefix apps/vscode run build',
    'node apps/vscode/scripts/selftest.mjs',
  ],
  jetbrains: [
    'bash scripts/check-gradle-integrity.sh',
    'bash -lc "cd apps/jetbrains && ./gradlew :frontend:buildPlugin --no-daemon"',
  ],
  ci: [
    'node scripts/programme/select-tests.mjs selftest',
    'node scripts/certification/check-workflow-graph.mjs',
    'sh scripts/check-ci-image-pins.sh',
  ],
  certification: [
    'node scripts/certification/evidence.mjs selftest',
    'node scripts/certification/lane-marker.mjs --selftest',
  ],
  contracts: ['node scripts/protocol-codegen.mjs --check-clients'],
  fixtures: ['cargo test -p faktor-tests-integration --all-features'],
  fuzz: ['cargo test -p faktor-tests-fuzz-seeds --all-features'],
  branding: ['bash scripts/branding-scan.sh'],
  scripts: ['node scripts/dogfood.mjs --coverage'],
  docs: ['bash scripts/check-docs-sync.sh'],
});

function suiteForPath(path) {
  for (const [prefix, suite] of SUITE_PATHS) {
    if (path === prefix.replace(/\/$/, '') || path.startsWith(prefix)) return suite;
  }
  return null;
}

/**
 * Select affected work for a changed-file list.
 *
 * @returns {{schema, mode, base, changed, crates, suites, commands, refused,
 *            unmapped, conservative}}
 */
function loadRegistryRows(root) {
  try {
    const document = JSON.parse(read(join(root, 'tests/programme/registry.json')));
    return Array.isArray(document.rows) ? document.rows : [];
  } catch {
    return [];
  }
}

function affectedSurfaces(rows, changed, graph, full) {
  if (full) return rows.map((row) => row.id).sort();
  const selected = new Set();
  for (const path of changed) {
    const crate = crateForPath(path, graph);
    for (const row of rows) {
      const rowPath = String(row.path ?? '').split('#')[0];
      if (
        rowPath === path ||
        rowPath.startsWith(`${path}/`) ||
        (crate !== null && (rowPath === crate.dir || rowPath.startsWith(`${crate.dir}/`)))
      ) {
        selected.add(row.id);
      }
    }
  }
  return [...selected].sort();
}

export function select(changedPaths, { mode = 'pull', base = null, root = ROOT } = {}) {
  const graph = workspaceGraph(root);
  const full = FULL_MODES.has(mode);
  const changed = [...new Set(changedPaths.map((path) => path.split('\\').join('/')))].sort();
  const crates = new Set();
  const suites = new Set();
  const unmapped = [];
  const notes = [];
  let forceFull = false;

  for (const path of changed) {
    if (path === 'Cargo.toml' || path === 'Cargo.lock' || path === 'rust-toolchain.toml') {
      forceFull = true;
      notes.push(`${path}: workspace manifest/toolchain change forces the full workspace`);
      continue;
    }
    const crate = crateForPath(path, graph);
    if (crate !== null) {
      crates.add(crate.name);
      continue;
    }
    const suite = suiteForPath(path);
    if (suite !== null) {
      suites.add(suite);
      continue;
    }
    // Root files that are read by the registry/docs/CI checks.
    if (/^(README\.md|AGENTS\.md|LICENSE|NOTICE|deny\.toml|\.gitignore)$/.test(path)) {
      suites.add('docs');
      suites.add('contracts');
      continue;
    }
    unmapped.push(path);
  }

  if (forceFull) {
    for (const crate of graph.crates.keys()) crates.add(crate);
    for (const suite of Object.keys(SUITE_COMMANDS)) suites.add(suite);
  }

  let selectedCrates;
  if (full) {
    selectedCrates = [...graph.crates.keys()].sort();
    for (const suite of Object.keys(SUITE_COMMANDS)) suites.add(suite);
    notes.push(`mode ${mode} is a full lane: every crate and suite is selected`);
  } else {
    selectedCrates = reverseClosure([...crates], graph);
    if (selectedCrates.length < crates.size) {
      notes.push('reverse-dependency closure applied to every changed crate');
    }
  }

  let deferredCrates = [];
  if (!full) {
    const kept = [];
    for (const name of selectedCrates) {
      if (Object.hasOwn(PULL_DEFERRED_CRATES, name)) {
        deferredCrates.push({ crate: name, lane: PULL_DEFERRED_CRATES[name] });
      } else {
        kept.push(name);
      }
    }
    selectedCrates = kept;
  }
  const commands = [];
  if (selectedCrates.length > 0) {
    commands.push(`cargo test -p ${selectedCrates.join(' -p ')} --all-features`);
    commands.push(`cargo check -p ${selectedCrates.join(' -p ')} --all-targets --all-features`);
  }
  for (const suite of [...suites].sort()) {
    for (const command of SUITE_COMMANDS[suite] ?? []) commands.push(command);
  }

  const empty = changed.length === 0;
  const refused = (unmapped.length > 0 || empty) && !full;
  if (empty) {
    notes.push(
      'empty diff/base unavailable: typed refusal; the caller must run the full relevant layer rather than silently test nothing',
    );
  }
  if (unmapped.length > 0) {
    notes.push(
      'unmapped changed path(s): typed refusal; the PR runner falls back to the full relevant layer (workspace tests) and records this selection as refused, never as a silent skip',
    );
  }

  const registryRows = loadRegistryRows(root);
  const surfaces = affectedSurfaces(registryRows, changed, graph, full);
  const statusById = new Map(registryRows.map((row) => [row.id, row.status]));
  const blockedSurfaces = surfaces.filter((id) => statusById.get(id) !== 'proven');
  return {
    schema: 'faktor-change-selection/v1',
    mode,
    base,
    head: git(['rev-parse', 'HEAD'], root),
    changed,
    crates: selectedCrates,
    deferred_crates: deferredCrates,
    suites: [...suites].sort(),
    commands: [...new Set(commands)],
    surfaces,
    blocked_surfaces: blockedSurfaces,
    unmapped: unmapped.sort(),
    refused,
    conservative: true,
    notes,
    digest: `sha256:${sha256(canonicalJson({ mode, changed, crates: selectedCrates, deferredCrates, suites: [...suites].sort(), commands, surfaces }))}`,
  };
}

function git(args, root) {
  try {
    return execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

function sha256(text) {
  return createHash('sha256').update(text).digest('hex');
}

/** Changed files between base and HEAD, including the working tree when asked. */
export function changedFiles({ base, worktree = false, root = ROOT } = {}) {
  const args = ['diff', '--name-only', '--diff-filter=ACMRD'];
  if (base) args.push(`${base}...HEAD`);
  else args.push('HEAD~1', 'HEAD');
  const committed = git(args, root);
  const files = committed === null ? [] : committed.split('\n').filter(Boolean);
  if (worktree) {
    for (const extra of ['diff', '--name-only', '--cached']) {
      const out = git([extra], root);
      if (out) files.push(...out.split('\n').filter(Boolean));
    }
    const status = git(['status', '--porcelain'], root);
    if (status) {
      for (const line of status.split('\n')) {
        if (line.length > 3) files.push(line.slice(3).trim());
      }
    }
  }
  return [...new Set(files)].sort();
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) {
      console.log(`selector selftest ok: ${name}`);
    } else {
      failures += 1;
      console.error(`selector selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };

  const store = select(['crates/store/src/ledger.rs']);
  check('changed store selects faktor-store', store.crates.includes('faktor-store'));
  check(
    'changed store selects reverse dependents (agent/server/cli)',
    ['faktor-agent', 'faktor-server', 'faktor-cli', 'faktor-session'].every((name) =>
      store.crates.includes(name),
    ),
    store.crates.join(','),
  );
  check(
    'selected crates emit one deterministic cargo test command',
    store.commands.some((command) => command.startsWith('cargo test -p ')),
  );

  check(
    'changed store maps to its own surface row',
    store.surfaces.includes('act-now.memory-pager-plan'),
    store.surfaces.join(','),
  );
  check(
    'blocked surfaces are reported as evidence, not hidden',
    store.blocked_surfaces.length >= 1,
  );

  const testCrate = select(['tests/quality/src/lib.rs']);
  check('changed test crate selects itself', testCrate.crates.includes('faktor-tests-quality'));

  const vscode = select(['apps/vscode/src/chat.ts']);
  check('VS Code change selects the vscode suite', vscode.suites.includes('vscode'));
  check('VS Code change does not select Rust crates', vscode.crates.length === 0);

  const edition8 = select(['docs/specs/edition8/flows.md']);
  check('Edition 8 spec change selects edition8 suite', edition8.suites.includes('edition8'));
  check(
    'Edition 8 suite runs the structural registry gate',
    edition8.commands.includes('node tests/edition8/registry.mjs'),
  );

  const ci = select(['.woodpecker/untrusted/pr.yaml']);
  check('CI change selects the ci suite', ci.suites.includes('ci'));

  const lock = select(['Cargo.lock']);
  check('Cargo.lock forces the full workspace', lock.crates.length > 40, `${lock.crates.length}`);

  const unmapped = select(['mystery.bin']);
  check('unmapped path is a typed refusal', unmapped.refused && unmapped.unmapped.includes('mystery.bin'));

  const trusted = select(['crates/store/src/ledger.rs'], { mode: 'trusted' });
  check('trusted mode selects every crate', trusted.crates.length === workspaceGraph().crates.size);
  check('trusted mode selects every suite', trusted.suites.length >= 8);
  const registryRows = JSON.parse(
    readFileSync(join(ROOT, 'tests/programme/registry.json'), 'utf8'),
  ).rows;
  check('trusted mode selects every registry surface', trusted.surfaces.length === registryRows.length);

  const first = JSON.stringify(select(['crates/core/src/lib.rs']));
  const second = JSON.stringify(select(['crates/core/src/lib.rs']));
  check('selection is deterministic', first === second);

  const core = select(['crates/core/src/lib.rs']);
  check(
    'a core change conservatively includes server and CLI',
    core.crates.includes('faktor-server') && core.crates.includes('faktor-cli'),
  );

  if (failures > 0) {
    console.error(`selector selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('selector selftest: PASS');
  return 0;
}

export function main() {
  const args = process.argv.slice(2);
  if (args.includes('selftest')) return selftest();
  const arg = (name, fallback = null) => {
    const index = args.indexOf(`--${name}`);
    return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
  };
  const mode = arg('mode', 'pull');
  if (!['pull', ...FULL_MODES].includes(mode)) {
    console.error(`selector: unknown mode '${mode}'`);
    return 2;
  }
  const explicit = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--changed-file' && index + 1 < args.length) {
      explicit.push(args[index + 1]);
    }
  }
  const files =
    explicit.length > 0
      ? [...new Set(explicit)].sort()
      : changedFiles({ base: arg('base'), worktree: args.includes('--worktree') });
  const selection = select(files, { mode, base: arg('base') });
  if (selection.refused && args.includes('--fallback-full')) {
    selection.commands.push('cargo test --workspace --all-features');
    selection.fell_back = 'full-workspace';
    selection.notes.push(
      'unmapped changed path(s): typed refusal escalated to the full workspace test; recorded explicitly',
    );
  }
  const text = `${JSON.stringify(selection, null, 2)}\n`;
  const out = arg('out');
  if (out) {
    mkdirSync(dirname(resolve(ROOT, out)), { recursive: true });
    writeFileSync(resolve(ROOT, out), text);
  }
  process.stdout.write(text);
  if (selection.refused && selection.fell_back === undefined) return 3;
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main();
}
