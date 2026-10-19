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
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { performance } from 'node:perf_hooks';
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
      members.push(pattern.replace(/\/+$/, ''));
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

const GRAPH_CACHE = new Map();

function manifestSignature(root, members) {
  const paths = [join(root, 'Cargo.toml')];
  for (const member of members) paths.push(join(root, member, 'Cargo.toml'));
  let signature = '';
  for (const path of paths) {
    try {
      const stat = statSync(path);
      signature += `${path}\0${stat.size}\0${stat.mtimeMs}\n`;
    } catch {
      signature += `${path}\0missing\n`;
    }
  }
  return signature;
}

/** Fixture hook: drop cached graphs for one root (or all roots). */
export function clearWorkspaceGraphCache(root = null) {
  if (root === null) GRAPH_CACHE.clear();
  else {
    const prefix = `${resolve(root)}\0`;
    for (const key of GRAPH_CACHE.keys()) if (key.startsWith(prefix)) GRAPH_CACHE.delete(key);
  }
}

/**
 * Crate metadata: name, directory, direct workspace dependencies and the
 * test/binary targets that `cargo test -p <name>` compiles.
 *
 * The graph (and its reverse edges) is cached per commit; a manifest-size/
 * mtime fingerprint keeps an uncommitted worktree edit from reusing a stale
 * graph. `refresh` bypasses the cache for fixture mutation.
 */
export function workspaceGraph(root = ROOT, { head = null, refresh = false } = {}) {
  const members = workspaceMembers(root);
  const key = `${resolve(root)}\0${head ?? ''}\0${manifestSignature(root, members)}`;
  if (!refresh) {
    const cached = GRAPH_CACHE.get(key);
    if (cached) return cached;
  }
  const crates = new Map();
  const byDir = [];
  const crateByDir = new Map();
  for (const member of members) {
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
    crateByDir.set(member, crate);
  }
  // Reverse dependency closure over workspace crates.
  const dependents = new Map();
  for (const crate of crates.values()) dependents.set(crate.name, new Set());
  for (const crate of crates.values()) {
    for (const dep of crate.deps) {
      if (dependents.has(dep)) dependents.get(dep).add(crate.name);
    }
  }
  const graph = { crates, byDir, crateByDir, dependents };
  GRAPH_CACHE.set(key, graph);
  return graph;
}

/**
 * Crate owning a repo-relative path (longest member directory prefix).
 * Walks the path's own prefixes against the directory index, so one lookup
 * is O(path depth) instead of O(crates).
 */
export function crateForPath(path, graph) {
  const normalized = path.split('\\').join('/').replace(/\/+$/, '');
  if (graph.crateByDir) {
    let candidate = normalized;
    while (candidate.length > 0) {
      const crate = graph.crateByDir.get(candidate);
      if (crate) return crate;
      const slash = candidate.lastIndexOf('/');
      if (slash < 0) break;
      candidate = candidate.slice(0, slash);
    }
    return null;
  }
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
  ['tests/lanes.toml', 'programme'],
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
    'node scripts/certification/check-workflow-graph.mjs --check',
    'sh scripts/check-ci-image-pins.sh',
  ],
  certification: [
    'node scripts/certification/evidence.mjs selftest',
    'node scripts/certification/check-workflow-graph.mjs selftest',
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
const REGISTRY_CACHE = new Map();
const REGISTRY_CACHE_LIMIT = 4;

/** Fixture hook: drop cached registry parses (or just one root's). */
export function clearRegistryCache(root = null) {
  if (root === null) REGISTRY_CACHE.clear();
  else {
    const prefix = `${resolve(root)}\0`;
    for (const key of REGISTRY_CACHE.keys()) if (key.startsWith(prefix)) REGISTRY_CACHE.delete(key);
  }
}

/**
 * Parse the committed registry once per (root, commit, file fingerprint).
 * A missing/corrupt registry is a typed error — never an empty row list that
 * would silently select no surfaces.
 */
function loadRegistryRows(root, head) {
  const path = join(root, 'tests/programme/registry.json');
  let stat;
  try {
    stat = statSync(path);
  } catch {
    return { rows: [], error: `${path} is missing` };
  }
  const key = `${resolve(root)}\0${head ?? ''}\0${stat.size}\0${stat.mtimeMs}`;
  const cached = REGISTRY_CACHE.get(key);
  if (cached) return cached;
  let loaded;
  try {
    const document = JSON.parse(read(path));
    if (!Array.isArray(document.rows)) loaded = { rows: [], error: 'registry has no rows array' };
    else if (document.rows.length === 0) loaded = { rows: [], error: 'registry has no rows' };
    else loaded = { rows: document.rows, error: null };
  } catch (error) {
    loaded = { rows: [], error: `registry is unreadable: ${error.message ?? error}` };
  }
  if (REGISTRY_CACHE.size >= REGISTRY_CACHE_LIMIT) {
    REGISTRY_CACHE.delete(REGISTRY_CACHE.keys().next().value);
  }
  REGISTRY_CACHE.set(key, loaded);
  return loaded;
}

/**
 * Index rows by every path prefix and by owning crate. Selecting then costs
 * O(changed-path depth + selected ids) instead of scanning every row for
 * every changed file (500 x 10k at the report's scale).
 */
function buildSurfaceIndex(rows, graph) {
  const byPrefix = new Map();
  const byCrate = new Map();
  const add = (map, key, id) => {
    let ids = map.get(key);
    if (ids === undefined) {
      ids = new Set();
      map.set(key, ids);
    }
    ids.add(id);
  };
  for (const row of rows) {
    const rowPath = String(row.path ?? '').split('#')[0].replace(/^\.\//, '');
    if (!rowPath) continue;
    add(byPrefix, rowPath, row.id);
    for (let index = rowPath.indexOf('/'); index !== -1; index = rowPath.indexOf('/', index + 1)) {
      add(byPrefix, rowPath.slice(0, index), row.id);
    }
    const crate = crateForPath(rowPath, graph);
    if (crate !== null) add(byCrate, crate.name, row.id);
  }
  return { byPrefix, byCrate };
}

function affectedSurfaces(index, rows, changed, graph, full) {
  if (full) return rows.map((row) => row.id).sort();
  if (changed.length === 0) return [];
  const selected = new Set();
  for (const path of changed) {
    for (const id of index.byPrefix.get(path) ?? []) selected.add(id);
    const crate = crateForPath(path, graph);
    if (crate !== null) {
      for (const id of index.byCrate.get(crate.name) ?? []) selected.add(id);
    }
  }
  return [...selected].sort();
}

export function select(changedPaths, { mode = 'pull', base = null, root = ROOT } = {}) {
  const head = git(['rev-parse', 'HEAD'], root);
  const graph = workspaceGraph(root, { head });
  const full = FULL_MODES.has(mode);
  // Directory spellings (`tests/programme/`) select their whole subtree; a
  // trailing slash must not turn a mapped path into an empty surface set.
  const changed = [
    ...new Set(
      changedPaths
        .map((path) => path.split('\\').join('/').replace(/\/+$/, ''))
        .filter((path) => path.length > 0),
    ),
  ].sort();
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
    if (
      /^(README\.md|AGENTS\.md|LICENSE|NOTICE|deny\.toml|\.gitignore|Report\.pdf)$/.test(
        path,
      )
    ) {
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

  const loaded = loadRegistryRows(root, head);
  // A manifest/toolchain change is a full-workspace selection in every
  // respect, including the surface set; the old path only widened crates and
  // suites, leaving surfaces derived from the changed file list.
  const wide = full || forceFull;
  const surfaces = loaded.error
    ? []
    : affectedSurfaces(
        wide || changed.length === 0 ? null : buildSurfaceIndex(loaded.rows, graph),
        loaded.rows,
        changed,
        graph,
        wide,
      );
  const statusById = new Map(loaded.rows.map((row) => [row.id, row.status]));
  const blockedSurfaces = surfaces.filter((id) => statusById.get(id) !== 'proven');
  const registryRefused = loaded.error !== null && !full;
  const empty = changed.length === 0;
  const refused = (unmapped.length > 0 || empty || registryRefused) && !full;
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
  if (registryRefused) {
    notes.push(
      `registry: ${loaded.error}: typed refusal; surface evidence is unavailable, so the caller must run the full relevant layer`,
    );
  }
  return {
    schema: 'faktor-change-selection/v1',
    mode,
    base,
    head,
    changed,
    crates: selectedCrates,
    deferred_crates: deferredCrates,
    suites: [...suites].sort(),
    commands: [...new Set(commands)],
    surfaces,
    blocked_surfaces: blockedSurfaces,
    unmapped: unmapped.sort(),
    refused,
    ...(loaded.error ? { registry_error: loaded.error } : {}),
    conservative: true,
    notes,
    digest: `sha256:${sha256(canonicalJson({ mode, changed, crates: selectedCrates, deferredCrates, suites: [...suites].sort(), commands, surfaces }))}`,
  };
}

function git(args, root) {
  try {
    return execFileSync('git', ['-C', root, ...args], {
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'ignore'],
    }).trim();
  } catch {
    return null;
  }
}

function sha256(text) {
  return createHash('sha256').update(text).digest('hex');
}

/** NUL-delimited name output: paths may contain spaces, quotes or newlines. */
function gitNames(args, root) {
  try {
    const out = execFileSync('git', ['-C', root, ...args], {
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
      stdio: ['ignore', 'pipe', 'ignore'],
    });
    return out
      .split('\0')
      .filter(Boolean)
      .map((path) => path.split('\\').join('/'));
  } catch {
    return null;
  }
}

/**
 * Changed files between base and HEAD, including the working tree when asked.
 * Every query asks git for NUL-delimited NAMES and disables rename detection
 * (a rename is a delete + add, so both paths select), so a patch body or a
 * quoted path can never leak into the selection.
 */
export function changedFiles({ base, worktree = false, root = ROOT } = {}) {
  const args = ['diff', '--name-only', '--no-renames', '--diff-filter=ACMRD', '-z'];
  if (base) args.push(`${base}...HEAD`);
  else args.push('HEAD~1', 'HEAD');
  const files = new Set(gitNames(args, root) ?? []);
  if (worktree) {
    for (const extra of [
      ['diff', '--name-only', '--no-renames', '--diff-filter=ACMRD', '-z'],
      ['diff', '--name-only', '--no-renames', '--diff-filter=ACMRD', '--cached', '-z'],
      ['ls-files', '--others', '--exclude-standard', '-z'],
    ]) {
      for (const path of gitNames(extra, root) ?? []) files.add(path);
    }
  }
  return [...files].sort();
}

function createScaleFixture({ crates = 50, rows = 10_000, changedPerCrate = 10 } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'faktor-selector-scale-'));
  mkdirSync(join(root, 'crates'), { recursive: true });
  const members = [];
  const names = [];
  for (let index = 0; index < crates; index += 1) {
    const suffix = String(index).padStart(2, '0');
    const dir = `crates/c${suffix}`;
    const name = `faktor-c${suffix}`;
    members.push(dir);
    names.push(name);
    mkdirSync(join(root, dir, 'src'), { recursive: true });
    const deps = [];
    if (index > 0) {
      const previous = String(index - 1).padStart(2, '0');
      deps.push(`faktor-c${previous} = { path = "../c${previous}" }`);
    }
    writeFileSync(
      join(root, dir, 'Cargo.toml'),
      `[package]\nname = "${name}"\nversion = "0.1.0"\n${deps.length > 0 ? `\n[dependencies]\n${deps.join('\n')}\n` : ''}`,
    );
    writeFileSync(join(root, dir, 'src/lib.rs'), '');
  }
  writeFileSync(
    join(root, 'Cargo.toml'),
    `[workspace]\nmembers = [${members.map((member) => `"${member}"`).join(', ')}]\n`,
  );
  const rowsPerCrate = Math.floor(rows / crates);
  const registryRows = [];
  for (let crate = 0; crate < crates; crate += 1) {
    const suffix = String(crate).padStart(2, '0');
    for (let index = 0; index < rowsPerCrate; index += 1) {
      registryRows.push({
        id: `surface.c${suffix}.${index}`,
        kind: 'native-route',
        path: `crates/c${suffix}/src/file${index}.rs`,
        layers: ['contracts'],
        lanes: ['pull', 'trusted'],
        test_ids: [`native.route-contract.c${suffix}.${index}`],
        evidence_kind: 'exact-value',
        oracle: 'malformed input is refused typed and never reaches the effect',
        adversarial: ['auth'],
        status: 'deferred',
      });
    }
  }
  mkdirSync(join(root, 'tests/programme'), { recursive: true });
  writeFileSync(
    join(root, 'tests/programme/registry.json'),
    `${JSON.stringify({ schema: 'faktor-test-programme-registry/v1', rows: registryRows })}\n`,
  );
  const changed = [];
  for (let crate = 0; crate < crates; crate += 1) {
    const suffix = String(crate).padStart(2, '0');
    for (let index = 0; index < changedPerCrate; index += 1) {
      changed.push(`crates/c${suffix}/src/changed${index}.rs`);
    }
  }
  return { root, members, names, rows: registryRows, changed };
}

function createWorktreeFixture() {
  const root = mkdtempSync(join(tmpdir(), 'faktor-selector-worktree-'));
  const git = (...args) =>
    execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  git('init', '-q');
  git('config', 'user.email', 'selector@example.invalid');
  git('config', 'user.name', 'selector');
  for (const file of ['tracked.txt', 'deleted.txt']) {
    writeFileSync(join(root, file), 'committed\n');
    git('add', file);
  }
  git('commit', '-qm', 'fixture');
  writeFileSync(join(root, 'tracked.txt'), 'modified\n');
  writeFileSync(join(root, 'staged.txt'), 'staged\n');
  git('add', 'staged.txt');
  writeFileSync(join(root, 'untracked.txt'), 'untracked\n');
  rmSync(join(root, 'deleted.txt'));
  return root;
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

  const lanes = select(['tests/lanes.toml']);
  check(
    'the lane manifest maps to the programme suite (never an unmapped authority)',
    lanes.suites.includes('programme') && lanes.refused === false,
  );
  const report = select(['Report.pdf']);
  check('a root review artifact maps to docs', report.suites.includes('docs') && report.refused === false);
  const directory = select(['tests/programme/']);
  check(
    'a trailing-slash directory selects its subtree surfaces',
    directory.refused === false && directory.surfaces.includes('act-now.event-ordering'),
    directory.surfaces.join(','),
  );

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

  const worktreeRoot = createWorktreeFixture();
  try {
    const worktreeFiles = changedFiles({ worktree: true, root: worktreeRoot });
    check(
      'worktree discovery returns paths only (no patch body / status prefix)',
      worktreeFiles.every(
        (path) =>
          !path.includes('\n') &&
          !path.startsWith('@@') &&
          !path.startsWith('+++') &&
          !path.startsWith('---') &&
          !path.startsWith(' ') &&
          !path.startsWith('diff '),
      ),
      worktreeFiles.slice(0, 4).join(' | '),
    );
    check(
      'worktree discovery covers modified, staged, untracked and deleted paths',
      ['tracked.txt', 'staged.txt', 'untracked.txt', 'deleted.txt'].every((path) =>
        worktreeFiles.includes(path),
      ),
      worktreeFiles.join(','),
    );
  } finally {
    rmSync(worktreeRoot, { recursive: true, force: true });
  }

  // Scale fixture: 500 changed files across 50 crates and 10k registry rows.
  // Selection must stay near-linear (indexed prefix/crate lookup) and must
  // return the complete conservative surface set.
  const scale = createScaleFixture();
  try {
    const heapBefore = process.memoryUsage().heapUsed;
    const scaleStart = performance.now();
    const scaled = select(scale.changed, { root: scale.root });
    const scaleMs = performance.now() - scaleStart;
    const heapMb = (process.memoryUsage().heapUsed - heapBefore) / 1048576;
    const expectedIds = scale.rows.map((row) => row.id);
    const actualIds = new Set(scaled.surfaces);
    const missing = expectedIds.filter((id) => !actualIds.has(id));
    check(
      '500-file diff across 50 crates selects the full reverse closure',
      scaled.crates.length === scale.names.length,
      `${scaled.crates.length}/${scale.names.length}`,
    );
    check(
      `500-file diff selects every row in every changed crate (${actualIds.size}/${expectedIds.length})`,
      missing.length === 0 && actualIds.size === expectedIds.length,
      `missing ${missing.length}`,
    );
    check(`500-file selection stays near-linear (${Math.round(scaleMs)} ms < 3000 ms)`, scaleMs < 3000);
    check(
      `500-file selection heap delta stays bounded (${Math.round(heapMb)} MB < 256 MB)`,
      heapMb < 256,
    );
    const chain = select(['crates/c00/src/file0.rs'], { root: scale.root });
    check(
      'reverse-dependency closure is transitive across the 50-crate chain',
      chain.crates.length === scale.names.length,
      `${chain.crates.length}/${scale.names.length}`,
    );
    const lockScale = select(['Cargo.lock'], { root: scale.root });
    check(
      'a manifest change selects every surface, not only crates/suites',
      lockScale.surfaces.length === expectedIds.length,
      `${lockScale.surfaces.length}/${expectedIds.length}`,
    );
    const unmappedScale = select(['mystery.bin'], { root: scale.root });
    check(
      'an unmapped path with a valid registry is still a typed refusal',
      unmappedScale.refused && unmappedScale.registry_error === undefined,
    );
    writeFileSync(join(scale.root, 'tests/programme/registry.json'), '{not json');
    clearRegistryCache(scale.root);
    const corrupt = select([scale.changed[0]], { root: scale.root });
    check(
      'a corrupt registry is a typed refusal, never a silently empty surface set',
      corrupt.refused === true && typeof corrupt.registry_error === 'string',
      JSON.stringify(corrupt.registry_error),
    );
  } finally {
    rmSync(scale.root, { recursive: true, force: true });
    clearWorkspaceGraphCache(scale.root);
    clearRegistryCache(scale.root);
  }

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
