#!/usr/bin/env node
// Change-aware test selection.
//
// Maps changed files (git diff vs a base commit, or an explicit file list) to
// affected crates, test crates and suites. The mapping is CONSERVATIVE: a
// changed crate selects the transitive closure of its reverse workspace
// dependencies, because a change in `faktor-store` can break `faktor-agent`,
// `faktor-server` and every test crate that depends on them.
//
// The crate/test/dependency map is the ONE serialized repository graph from
// `faktor-impact` (`faktor impact graph --selector`), produced by the same
// builder the native semantic `affected()` provider consumes. A missing,
// stale, unsupported or corrupt graph is a typed refusal with a conservative
// full-lane fallback; this file never re-derives a private manifest graph.
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

import { execFileSync, spawnSync } from 'node:child_process';
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

export const IMPACT_GRAPH_SCHEMA = 'faktor-impact-graph/v1';

function graphCandidates(root, explicit = null) {
  const paths = [];
  if (explicit) paths.push(explicit);
  if (process.env.FAKTOR_IMPACT_GRAPH) paths.push(process.env.FAKTOR_IMPACT_GRAPH);
  paths.push(join(root, 'target/certification/impact/graph.json'));
  paths.push(join(root, 'tests/programme/impact-graph.json'));
  return [...new Set(paths.map((path) => resolve(root, path)))];
}

function readJson(path) {
  const text = read(path);
  return JSON.parse(text);
}

const VALIDATOR_ENV = 'FAKTOR_IMPACT_VALIDATOR';
const VERDICT_SCHEMA = 'faktor-impact-validation/v1';
const VALIDATOR_TIMEOUT_MS = 120_000;
const VALIDATOR_MAX_BUFFER = 4 * 1024 * 1024;
let VALIDATOR = null;
let VALIDATOR_RESOLVED = false;
const VALIDATION_CACHE = new Map();
const VALIDATION_CACHE_LIMIT = 16;

/**
 * Resolve the Rust authority. Explicit env override, then a built
 * `faktor-ci-tools` binary, then `cargo run`. Resolution is cached: the
 * selector never reimplements graph validation in JavaScript.
 */
function validatorCommand() {
  if (VALIDATOR_RESOLVED) return VALIDATOR;
  VALIDATOR_RESOLVED = true;
  const explicit = process.env[VALIDATOR_ENV];
  if (explicit) {
    VALIDATOR = { cmd: explicit, args: [], label: explicit };
    return VALIDATOR;
  }
  for (const profile of ['debug', 'release']) {
    const candidate = join(ROOT, 'target', profile, process.platform === 'win32' ? 'faktor-ci-tools.exe' : 'faktor-ci-tools');
    if (existsSync(candidate)) {
      VALIDATOR = { cmd: candidate, args: [], label: candidate };
      return VALIDATOR;
    }
  }
  try {
    execFileSync('cargo', ['--version'], { stdio: ['ignore', 'ignore', 'ignore'] });
    VALIDATOR = {
      cmd: 'cargo',
      args: ['run', '-q', '--manifest-path', join(ROOT, 'Cargo.toml'), '-p', 'faktor-ci-tools', '--'],
      label: 'cargo run -p faktor-ci-tools',
    };
  } catch {
    VALIDATOR = null;
  }
  return VALIDATOR;
}

function runValidator(args, input = null) {
  const validator = validatorCommand();
  if (validator === null) {
    return {
      ok: false,
      unavailable: true,
      code: 'validator_unavailable',
      error: `impact graph validator unavailable: build faktor-ci-tools or set ${VALIDATOR_ENV}`,
    };
  }
  const result = spawnSync(validator.cmd, [...validator.args, 'impact', 'validate', '--json', ...args], {
    encoding: 'utf8',
    input: input ?? undefined,
    maxBuffer: VALIDATOR_MAX_BUFFER,
    timeout: VALIDATOR_TIMEOUT_MS,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  if (result.error) {
    return {
      ok: false,
      unavailable: true,
      code: 'validator_unavailable',
      error: `impact graph validator unavailable (${validator.label}): ${result.error.message ?? result.error}`,
    };
  }
  let verdict;
  try {
    verdict = JSON.parse(String(result.stdout ?? '').trim());
  } catch {
    return {
      ok: false,
      unavailable: false,
      code: 'validator_malformed',
      error: `impact graph validator emitted no verdict (exit ${result.status}): ${String(result.stderr ?? '').trim().slice(0, 300)}`,
    };
  }
  if (verdict.schema !== VERDICT_SCHEMA) {
    return {
      ok: false,
      unavailable: false,
      code: 'validator_malformed',
      error: `impact graph validator verdict schema ${JSON.stringify(verdict.schema)} is unsupported`,
    };
  }
  return verdict;
}

function validateGraphText(text, head = null, { root = ROOT } = {}) {
  const args = ['--stdin', '--root', resolve(root)];
  if (head) args.push('--head', String(head));
  const key = `${resolve(root)}\0${head ?? ''}\0${sha256(text)}`;
  const cached = VALIDATION_CACHE.get(key);
  if (cached) return cached;
  const verdict = runValidator(args, text);
  if (VALIDATION_CACHE.size >= VALIDATION_CACHE_LIMIT) {
    VALIDATION_CACHE.delete(VALIDATION_CACHE.keys().next().value);
  }
  if (!verdict.unavailable) VALIDATION_CACHE.set(key, verdict);
  return verdict;
}

/** Compute a canonical Rust-authority seal (used only for fixtures). */
export function sealImpactGraph(document) {
  const validator = validatorCommand();
  if (validator === null) {
    throw new Error(`impact graph validator unavailable: build faktor-ci-tools or set ${VALIDATOR_ENV}`);
  }
  const sealed = spawnSync(validator.cmd, [...validator.args, 'impact', 'seal', '--stdin'], {
    encoding: 'utf8',
    input: `${JSON.stringify(document)}\n`,
    maxBuffer: 64 * 1024 * 1024,
    timeout: VALIDATOR_TIMEOUT_MS,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  if (sealed.error || sealed.status !== 0) {
    throw new Error(
      `impact graph seal failed: ${sealed.error?.message ?? String(sealed.stderr ?? '').trim().slice(0, 300)}`,
    );
  }
  return JSON.parse(String(sealed.stdout).trim());
}

/**
 * Load the ONE serialized repository graph. The graph bytes are handed to
 * the Rust authority (`faktor-ci-tools impact validate`); this module never
 * reimplements schema/order/dangling/reverse/digest checks. A missing,
 * stale, unsupported or corrupt graph is a typed error and the caller falls
 * back conservatively (whole workspace), never to a second manifest
 * heuristic.
 */
export function loadImpactGraph(root = ROOT, { head = null, path = null } = {}) {
  const candidates = graphCandidates(root, path);
  for (const candidate of candidates) {
    if (!existsSync(candidate)) continue;
    let text;
    try {
      text = read(candidate);
    } catch (error) {
      return { document: null, error: `impact graph is unreadable: ${error.message ?? error}`, path: candidate, code: 'io' };
    }
    const verdict = validateGraphText(text, head, { root });
    if (!verdict.ok) {
      return { document: null, error: verdict.error, code: verdict.code, path: candidate };
    }
    let document;
    try {
      document = JSON.parse(text);
    } catch (error) {
      return { document: null, error: `impact graph is unreadable: ${error.message ?? error}`, path: candidate, code: 'corrupt_json' };
    }
    return { document, error: null, code: null, path: candidate };
  }
  return { document: null, error: 'impact graph is missing (generator: scripts/programme/build-impact-graph.mjs)', code: 'missing', path: null };
}

const GRAPH_FILE = 'tests/programme/impact-graph.json';

/**
 * Validate a document through the Rust authority. Returns `null` when the
 * authority accepts it, otherwise the typed error text. Exported for the
 * generator/selftests and for callers that already hold a parsed document.
 */
export function validateImpactGraph(document, head = null, { root = ROOT } = {}) {
  let text;
  try {
    text = JSON.stringify(document);
  } catch (error) {
    return `impact graph is unreadable: ${error.message ?? error}`;
  }
  const verdict = validateGraphText(text, head, { root });
  return verdict.ok ? null : verdict.error;
}

function authorityModel(document) {
  const crates = new Map();
  const packages = new Map();
  const units = new Map();
  const crateByDir = new Map();
  const dependents = new Map();
  const testsByCrate = new Map();
  const nodesById = new Map();
  const reverse = new Map();
  const files = new Map();
  const fileCrate = new Map();
  const byPath = new Map();
  const unitIdByPath = new Map();
  for (const node of document.nodes) {
    nodesById.set(node.id, node);
    if (node.path) {
      if (!byPath.has(node.path)) byPath.set(node.path, []);
      byPath.get(node.path).push(node.id);
    }
    if (node.kind === 'crate') {
      const dir = node.path === '.' || node.path === undefined ? '' : String(node.path).replace(/\/+$/, '');
      const unit = { name: node.name, dir, kind: 'crate', nodeId: node.id, deps: new Set() };
      crates.set(node.name, unit);
      units.set(node.name, unit);
      if (dir.length > 0) {
        crateByDir.set(dir, unit);
        unitIdByPath.set(dir, node.id);
      }
    }
    // A package node WITH a path is a local unit (npm/Gradle/Maven and
    // other user-language packages); a package node without a path is an
    // external dependency.
    if (node.kind === 'package' && node.path) {
      const dir = node.path === '.' ? '' : String(node.path).replace(/\/+$/, '');
      const unit = { name: node.name, dir, kind: 'package', nodeId: node.id, deps: new Set() };
      packages.set(node.name, unit);
      units.set(node.name, unit);
      if (dir.length > 0) {
        crateByDir.set(dir, unit);
        unitIdByPath.set(dir, node.id);
      }
    }
    if (node.kind === 'file' && node.path) {
      files.set(node.path, node.id);
      if (node.crate_name) fileCrate.set(node.path, node.crate_name);
    }
    if (node.kind === 'test') {
      const crate = node.crate_name;
      if (crate) {
        if (!testsByCrate.has(crate)) testsByCrate.set(crate, []);
        testsByCrate.get(crate).push(node.id);
      }
    }
  }
  for (const unit of units.values()) dependents.set(unit.name, new Set());
  for (const edge of document.edges) {
    if (edge.kind === 'depends_on') {
      const from = nodesById.get(edge.from);
      const to = nodesById.get(edge.to);
      if (
        from &&
        to &&
        ['crate', 'package'].includes(from.kind) &&
        ['crate', 'package'].includes(to.kind)
      ) {
        const unit = units.get(from.name);
        const dependency = units.get(to.name);
        if (unit && dependency && unit !== dependency) {
          unit.deps.add(dependency.name);
          dependents.get(dependency.name).add(unit.name);
        }
      }
    }
  }
  for (const edge of document.reverse_edges) {
    if (!reverse.has(edge.from)) reverse.set(edge.from, []);
    reverse.get(edge.from).push(edge);
  }
  for (const tests of testsByCrate.values()) tests.sort();
  const byDir = [...units.values()].sort((a, b) => b.dir.length - a.dir.length || a.dir.localeCompare(b.dir));
  return {
    crates,
    packages,
    units,
    byDir,
    crateByDir,
    unitIdByPath,
    dependents,
    testsByCrate,
    nodesById,
    reverse,
    files,
    fileCrate,
    byPath,
    document,
  };
}

/**
 * Reverse traversal over the SAME serialized graph the native authority
 * uses. Crate nodes propagate to dependent crates and their tests, exactly
 * like `faktor-impact`'s query; other nodes follow the materialized reverse
 * edges with a depth bound. Deterministic, bounded, and conservative.
 */
export function impactFromAuthority(authority, changed, { maxDepth = 8, maxResults = 4096 } = {}) {
  const seeds = new Set();
  for (const path of changed) {
    const file = authority.files.get(path);
    if (file) seeds.add(file);
    // Symbols/tests defined by the changed file are seeds too: coverage
    // edges then point from tests to exactly those declarations.
    for (const id of authority.byPath.get(path) ?? []) seeds.add(id);
    const unit = crateForPath(path, authority);
    if (unit) seeds.add(unit.nodeId ?? `${unit.kind}:${unit.name}`);
  }
  const best = new Map();
  const queue = [];
  const push = (id, distance, reason) => {
    const current = best.get(id);
    if (current && current.distance <= distance) return;
    best.set(id, { distance, reason });
    queue.push(id);
  };
  for (const id of [...seeds].sort()) push(id, 0, 'changed');
  const tests = [];
  const crates = new Set();
  let visitedEdges = 0;
  let cursor = 0;
  while (cursor < queue.length) {
    const id = queue[cursor];
    cursor += 1;
    const current = best.get(id);
    if (!current || current.distance >= maxDepth) continue;
    const node = authority.nodesById.get(id);
    if (!node) continue;
    if (node.kind === 'crate') crates.add(node.name);
    if (['crate', 'package'].includes(node.kind)) {
      for (const test of authority.testsByCrate.get(node.name) ?? []) {
        push(test, current.distance + 1, `tests:${node.name}`);
      }
    }
    for (const edge of authority.reverse.get(id) ?? []) {
      visitedEdges += 1;
      if (['crate', 'package'].includes(node.kind) && !['depends_on', 'tests'].includes(edge.kind)) continue;
      if (node.kind === 'file' && edge.kind === 'defines') continue;
      push(edge.to, current.distance + 1, `${edge.kind}:${id}`);
    }
  }
  for (const [id, value] of best) {
    const node = authority.nodesById.get(id);
    if (node?.kind !== 'test') continue;
    tests.push({
      id,
      name: node.name,
      path: node.path ?? null,
      crate: node.crate_name ?? null,
      reason: value.reason,
      distance: value.distance,
    });
  }
  tests.sort((a, b) => a.id.localeCompare(b.id));
  const truncated = tests.length > maxResults;
  return {
    tests: tests.slice(0, maxResults),
    crates: [...crates].sort(),
    visited_edges: visitedEdges,
    truncated,
  };
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
 * The workspace graph consumed by selection: the serialized authority
 * document, with reverse crate edges, crate directories and test nodes.
 * `authority: false` means the caller MUST fall back conservatively.
 */
export function workspaceGraph(root = ROOT, { head = null, refresh = false, path = null } = {}) {
  const loaded = loadImpactGraph(root, { head, path });
  const key = `${resolve(root)}\0${head ?? ''}\0${loaded.path ?? ''}\0${loaded.error ?? ''}`;
  if (!refresh) {
    const cached = GRAPH_CACHE.get(key);
    if (cached) return cached;
  }
  const graph = loaded.document
    ? { ...authorityModel(loaded.document), authority: true, source: 'authority', error: null, code: null, revision: loaded.document.revision ?? null, digest: loaded.document.digest, path: loaded.path }
    : { crates: new Map(), packages: new Map(), units: new Map(), byDir: [], crateByDir: new Map(), dependents: new Map(), testsByCrate: new Map(), nodesById: new Map(), reverse: new Map(), files: new Map(), fileCrate: new Map(), byPath: new Map(), document: null, authority: false, source: 'conservative-fallback', error: loaded.error, code: loaded.code ?? null, revision: null, digest: null, path: null };
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
    const exact = graph.fileCrate?.get(normalized);
    if (exact) return graph.crates.get(exact) ?? null;
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
  return [...selected].filter((name) => graph.units?.has(name) ?? graph.crates.has(name)).sort();
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

function manifestCrateNames(root) {
  const names = [];
  for (const member of workspaceMembers(root)) {
    try {
      const text = read(join(root, member, 'Cargo.toml'));
      const name = /\[package\][\s\S]*?^name\s*=\s*"([^"]+)"/m.exec(text)?.[1];
      if (name) names.push(name);
    } catch {
      // A member without a readable manifest is skipped visibly by the
      // caller's full-lane command, never fabricated as a selectable crate.
    }
  }
  return [...new Set(names)].sort();
}

function testCommandsFor(tests, limit = 256, graph = null) {
  const commands = new Set();
  for (const test of tests.slice(0, limit)) {
    if (!test.crate || !test.name) continue;
    // Only Cargo crates have `cargo test -p` commands; npm/Gradle/Maven and
    // other user-language units are still reported in `tests` and select
    // their surfaces.
    if (graph && !graph.crates.has(test.crate)) continue;
    if (!/^[A-Za-z0-9_:.-]+$/.test(test.name)) continue;
    if (!/^[A-Za-z0-9_-]+$/.test(test.crate)) continue;
    const path = String(test.path ?? '');
    const marker = path.lastIndexOf('/tests/');
    if (marker >= 0) {
      const file = path.slice(marker + '/tests/'.length);
      if (!file.endsWith('.rs') || file.includes('/')) continue;
      const target = file.slice(0, -3);
      commands.add(`cargo test -p ${test.crate} --test ${target} ${test.name}`);
    } else {
      commands.add(`cargo test -p ${test.crate} --lib ${test.name}`);
    }
  }
  return [...commands].sort();
}

export function select(changedPaths, { mode = 'pull', base = null, root = ROOT, graphPath = null } = {}) {
  const head = git(['rev-parse', 'HEAD'], root);
  const graph = workspaceGraph(root, { head, path: graphPath });
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
  const changedPackages = new Set();
  const suites = new Set();
  const unmapped = [];
  const notes = [];
  let forceFull = false;
  const graphFallback = !full && !graph.authority;
  if (graphFallback) {
    notes.push(
      `repository graph unavailable (${graph.error}); typed refusal with a conservative full-lane fallback`,
    );
  }

  for (const path of changed) {
    if (path === 'Cargo.toml' || path === 'Cargo.lock' || path === 'rust-toolchain.toml') {
      forceFull = true;
      notes.push(`${path}: workspace manifest/toolchain change forces the full workspace`);
      continue;
    }
    const crate = crateForPath(path, graph);
    if (crate !== null) {
      if (crate.kind === 'crate') crates.add(crate.name);
      else changedPackages.add(crate.name);
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
    for (const unit of graph.packages?.keys() ?? []) changedPackages.add(unit);
    for (const suite of Object.keys(SUITE_COMMANDS)) suites.add(suite);
  }

  let selectedCrates;
  if (full || (graphFallback && (forceFull || changed.length === 0))) {
    selectedCrates = [...graph.crates.keys()].sort();
    if (selectedCrates.length === 0) selectedCrates = manifestCrateNames(root);
    for (const suite of Object.keys(SUITE_COMMANDS)) suites.add(suite);
    notes.push(
      full
        ? `mode ${mode} is a full lane: every crate and suite is selected`
        : 'repository graph unavailable: every workspace crate and suite is selected conservatively',
    );
  } else if (graphFallback) {
    selectedCrates = [];
  } else {
    const closure = reverseClosure([...crates, ...changedPackages], graph);
    selectedCrates = closure.filter((name) => graph.crates.has(name));
    for (const name of closure) {
      if (graph.packages?.has(name)) changedPackages.add(name);
    }
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
  const impact = graph.authority && changed.length > 0
    ? impactFromAuthority(graph, changed, { maxDepth: 8, maxResults: 4096 })
    : { tests: [], crates: [], visited_edges: 0, truncated: false };
  const testCommands = full || forceFull || graphFallback ? [] : testCommandsFor(impact.tests, 16, graph);
  const commands = [];
  if (selectedCrates.length > 0) {
    commands.push(`cargo test -p ${selectedCrates.join(' -p ')} --all-features`);
    commands.push(`cargo check -p ${selectedCrates.join(' -p ')} --all-targets --all-features`);
  }
  for (const command of testCommands) commands.push(command);
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
  const refused = (unmapped.length > 0 || empty || registryRefused || graphFallback) && !full;
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
  if (impact.truncated) {
    notes.push('impacted test set reached the graph bound; crate-level tests remain the conservative floor');
  }
  if (impact.tests.length > testCommands.length) {
    notes.push(
      `impacted tests: ${impact.tests.length} selected from the graph; ${testCommands.length} per-test commands previewed (crate tests remain the floor)`,
    );
  }
  return {
    schema: 'faktor-change-selection/v1',
    mode,
    base,
    head,
    changed,
    crates: selectedCrates,
    packages: [...changedPackages].sort(),
    deferred_crates: deferredCrates,
    suites: [...suites].sort(),
    commands: [...new Set(commands)],
    surfaces,
    blocked_surfaces: blockedSurfaces,
    unmapped: unmapped.sort(),
    refused,
    tests: impact.tests,
    test_commands: testCommands,
    graph_source: graph.source,
    graph_revision: graph.revision?.commit ?? null,
    graph_digest: graph.digest,
    impacted_crates: impact.crates,
    ...(graph.error
      ? { graph_error: graph.error, graph_error_code: graph.code ?? null }
      : {}),
    ...(loaded.error ? { registry_error: loaded.error } : {}),
    conservative: true,
    notes,
    digest: `sha256:${sha256(canonicalJson({ mode, changed, crates: selectedCrates, deferredCrates, suites: [...suites].sort(), commands, surfaces, graphSource: graph.source, graphRevision: graph.revision?.commit ?? null, tests: impact.tests }))}`,
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

function writeImpactGraphFixture(root, { crateNames = [], testsPerCrate = 0 } = {}) {
  const nodes = [];
  const edges = [];
  for (const [index, name] of crateNames.entries()) {
    const suffix = String(index).padStart(2, '0');
    nodes.push({
      id: `crate:${name}`,
      kind: 'crate',
      name,
      path: `crates/c${suffix}`,
      provenance: 'manifest',
    });
    if (testsPerCrate > 0) {
      const testId = `test:crates/c${suffix}/tests/smoke.rs#smoke${suffix}@1`;
      nodes.push({
        id: testId,
        kind: 'test',
        name: `smoke${suffix}`,
        path: `crates/c${suffix}/tests/smoke.rs`,
        crate_name: name,
        line: 1,
        provenance: 'coverage',
      });
      edges.push({ from: testId, to: `crate:${name}`, kind: 'tests', provenance: 'coverage' });
    }
    if (index > 0) {
      edges.push({
        from: `crate:${name}`,
        to: `crate:${crateNames[index - 1]}`,
        kind: 'depends_on',
        provenance: 'manifest',
      });
    }
  }
  nodes.sort((a, b) => a.id.localeCompare(b.id));
  edges.sort((a, b) => `${a.from}\0${a.kind}\0${a.to}`.localeCompare(`${b.from}\0${b.kind}\0${b.to}`));
  const reverse_edges = edges
    .map((edge) => ({ ...edge, from: edge.to, to: edge.from }))
    .sort((a, b) => `${a.from}\0${a.kind}\0${a.to}`.localeCompare(`${b.from}\0${b.kind}\0${b.to}`));
  const document = sealImpactGraph({
    schema: IMPACT_GRAPH_SCHEMA,
    revision: { commit: null, tree: null, dirty: false },
    workspace: 1,
    files: [],
    nodes,
    edges,
    reverse_edges,
    stats: { nodes: nodes.length, edges: edges.length, reverse_edges: reverse_edges.length },
    digest: '',
  });
  mkdirSync(join(root, 'tests/programme'), { recursive: true });
  writeFileSync(join(root, 'tests/programme/impact-graph.json'), `${JSON.stringify(document)}\n`);
  clearWorkspaceGraphCache(root);
  return document;
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
  writeImpactGraphFixture(root, { crateNames: names, testsPerCrate: 1 });
  return { root, members, names, rows: registryRows, changed };
}

function createBindingFixture() {
  const root = mkdtempSync(join(tmpdir(), 'faktor-selector-binding-'));
  mkdirSync(join(root, 'crates/a/src'), { recursive: true });
  writeFileSync(join(root, 'Cargo.toml'), '[workspace]\nmembers = ["crates/a"]\n');
  writeFileSync(join(root, 'crates/a/Cargo.toml'), '[package]\nname = "a"\nversion = "0.1.0"\n');
  writeFileSync(join(root, 'crates/a/src/lib.rs'), '');
  const git = (...args) =>
    execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  git('init', '-q');
  git('config', 'user.email', 'selector@example.invalid');
  git('config', 'user.name', 'selector');
  git('add', '.');
  git('commit', '-qm', 'base');
  const base = git('rev-parse', 'HEAD').trim();
  const fixture = writeImpactGraphFixture(root, { crateNames: ['a'] });
  const document = sealImpactGraph({
    ...fixture,
    revision: { commit: base, tree: null, dirty: false },
  });
  writeFileSync(join(root, 'tests/programme/impact-graph.json'), `${JSON.stringify(document)}\n`);
  clearWorkspaceGraphCache(root);
  git('add', '.');
  git('commit', '-qm', 'graph at base');
  return { root, base, git, document };
}

function createRootSourceFixture({ manifest = true } = {}) {
  const root = mkdtempSync(join(tmpdir(), 'faktor-selector-root-'));
  const git = (...args) =>
    execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  git('init', '-q');
  git('config', 'user.email', 'selector@example.invalid');
  git('config', 'user.name', 'selector');
  if (manifest) writeFileSync(join(root, 'package.json'), '{"name":"root","private":true}\n');
  mkdirSync(join(root, 'lib'), { recursive: true });
  writeFileSync(join(root, 'lib/engine.ts'), 'export const engine = 1;\n');
  git('add', '.');
  git('commit', '-qm', 'base');
  const base = git('rev-parse', 'HEAD').trim();
  const document = sealImpactGraph({
    schema: IMPACT_GRAPH_SCHEMA,
    revision: { commit: base, tree: null, dirty: false },
    workspace: 1,
    files: [],
    nodes: [
      {
        id: 'package:root',
        kind: 'package',
        name: 'root',
        path: '.',
        provenance: 'manifest',
      },
    ],
    edges: [],
    reverse_edges: [],
    stats: {},
    digest: '',
  });
  writeSelectorGraph(root, document);
  git('add', '.');
  git('commit', '-qm', 'graph at base');
  clearWorkspaceGraphCache(root);
  return { root, base, git, document };
}

function writeSelectorGraph(root, document) {
  mkdirSync(join(root, 'tests/programme'), { recursive: true });
  writeFileSync(join(root, 'tests/programme/impact-graph.json'), `${JSON.stringify(document)}\n`);
  clearWorkspaceGraphCache(root);
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

  // The graph is committed in the same commit it describes, so its revision
  // binding is the parent commit. It stays valid while non-input commits land
  // on top; any graph input change makes it a typed refusal.
  const binding = createBindingFixture();
  try {
    mkdirSync(join(binding.root, 'docs'), { recursive: true });
    writeFileSync(join(binding.root, 'docs/note.md'), 'docs\n');
    binding.git('add', '.');
    binding.git('commit', '-qm', 'docs only');
    const docsHead = binding.git('rev-parse', 'HEAD').trim();
    const accepted = loadImpactGraph(binding.root, { head: docsHead });
    check(
      'a clean graph committed with its revision is accepted after non-input commits',
      accepted.error === null,
      accepted.error ?? '',
    );
    writeFileSync(join(binding.root, 'crates/a/src/lib.rs'), '// changed\n');
    binding.git('add', '.');
    binding.git('commit', '-qm', 'rust input');
    const codeHead = binding.git('rev-parse', 'HEAD').trim();
    const rejected = loadImpactGraph(binding.root, { head: codeHead });
    check(
      'a graph whose revision diff touches graph inputs is a typed refusal',
      /graph inputs changed/.test(rejected.error ?? ''),
      JSON.stringify(rejected.error),
    );
    const dirty = sealImpactGraph({
      ...structuredClone(binding.document),
      revision: { ...binding.document.revision, dirty: true },
    });
    check(
      'a dirty graph bound to another revision is a typed refusal',
      /dirty tree/.test(validateImpactGraph(dirty, docsHead, { root: binding.root }) ?? ''),
    );
  } finally {
    rmSync(binding.root, { recursive: true, force: true });
    clearWorkspaceGraphCache(binding.root);
  }

  // A root package with `lib/` sources (not `src/`) and a generic
  // manifestless repository must both mark their sources material. The
  // selector consumes the Rust verdict verbatim; it never decides freshness
  // with a second path heuristic.
  for (const [label, options] of [
    ['a root package lib source', { manifest: true }],
    ['a manifestless generic source', { manifest: false }],
  ]) {
    const fixture = createRootSourceFixture(options);
    try {
      writeFileSync(join(fixture.root, 'lib/engine.ts'), 'export const engine = 2;\n');
      fixture.git('add', '.');
      fixture.git('commit', '-qm', 'source input');
      const head = fixture.git('rev-parse', 'HEAD').trim();
      const refused = loadImpactGraph(fixture.root, { head });
      const direct = validateImpactGraph(fixture.document, head, { root: fixture.root });
      check(
        `${label} change is a typed refusal`,
        /graph inputs changed/.test(refused.error ?? ''),
        JSON.stringify(refused.error),
      );
      check(
        `validator and selector agree on ${label} change`,
        refused.error !== null && refused.error === direct && refused.code === 'stale_revision',
        JSON.stringify({ refused, direct }),
      );
      const selection = select(['lib/engine.ts'], { root: fixture.root });
      check(
        `${label} never selects from a stale graph`,
        selection.refused === true &&
          selection.graph_source === 'conservative-fallback' &&
          /graph inputs changed/.test(selection.graph_error ?? ''),
        JSON.stringify(selection.graph_error),
      );
      writeSelectorGraph(
        fixture.root,
        sealImpactGraph({
          ...structuredClone(fixture.document),
          revision: { commit: head, tree: null, dirty: false },
        }),
      );
      writeFileSync(join(fixture.root, 'package.json'), '{"name":"root","private":true,"version":"2"}\n');
      fixture.git('add', '.');
      fixture.git('commit', '-qm', 'manifest input');
      const manifestHead = fixture.git('rev-parse', 'HEAD').trim();
      const manifestRefused = loadImpactGraph(fixture.root, { head: manifestHead });
      check(
        `${label} repository manifest change is a typed refusal`,
        /graph inputs changed/.test(manifestRefused.error ?? '') &&
          manifestRefused.error ===
            validateImpactGraph(
              sealImpactGraph({
                ...structuredClone(fixture.document),
                revision: { commit: head, tree: null, dirty: false },
              }),
              manifestHead,
              { root: fixture.root },
            ),
        JSON.stringify(manifestRefused.error),
      );
    } finally {
      rmSync(fixture.root, { recursive: true, force: true });
      clearWorkspaceGraphCache(fixture.root);
    }
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
    check(
      'selection consumes the repository graph authority',
      scaled.graph_source === 'authority' && scaled.graph_revision === null,
      `${scaled.graph_source}:${scaled.graph_error ?? ''}`,
    );
    check(
      'impacted tests propagate across the 50-crate reverse chain',
      scaled.tests.length === scale.names.length,
      `${scaled.tests.length}/${scale.names.length}`,
    );
    check(
      'impacted tests emit per-test commands from the same graph',
      scaled.test_commands.some((command) => command.includes('--test smoke')),
      scaled.test_commands.slice(0, 2).join(' | '),
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
    writeFileSync(join(scale.root, 'tests/programme/impact-graph.json'), '{not json');
    clearWorkspaceGraphCache(scale.root);
    const corruptGraph = select([scale.changed[0]], { root: scale.root });
    check(
      'a corrupt graph is a typed refusal with conservative fallback, never a silent heuristic',
      corruptGraph.refused === true &&
        corruptGraph.graph_source === 'conservative-fallback' &&
        typeof corruptGraph.graph_error === 'string',
      JSON.stringify(corruptGraph.graph_error),
    );
    writeImpactGraphFixture(scale.root, { crateNames: scale.names, testsPerCrate: 1 });
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

  const stalePath = join(tmpdir(), `faktor-impact-stale-${process.pid}.json`);
  try {
    const document = sealImpactGraph({
      ...JSON.parse(readFileSync(join(ROOT, 'tests/programme/impact-graph.json'), 'utf8')),
      revision: { commit: 'deadbeef', tree: null, dirty: false },
    });
    writeFileSync(stalePath, `${JSON.stringify(document)}\n`);
    clearWorkspaceGraphCache(ROOT);
    const stale = select(['crates/core/src/lib.rs'], { graphPath: stalePath });
    check(
      'a graph bound to another revision is a typed refusal, never trusted',
      stale.refused === true &&
        stale.graph_source === 'conservative-fallback' &&
        /stale/.test(stale.graph_error ?? ''),
      JSON.stringify(stale.graph_error),
    );
  } finally {
    rmSync(stalePath, { force: true });
    clearWorkspaceGraphCache(ROOT);
  }

  // Single-validator agreement: every structural mutation is refused by
  // the Rust authority, and the selector consumes that verdict verbatim.
  const validatorRoot = mkdtempSync(join(tmpdir(), 'faktor-selector-validator-'));
  try {
    const base = sealImpactGraph({
      schema: IMPACT_GRAPH_SCHEMA,
      revision: { commit: null, tree: null, dirty: false },
      workspace: 1,
      files: [],
      nodes: [
        { id: 'crate:a', kind: 'crate', name: 'a', path: 'a', provenance: 'manifest' },
        { id: 'test:t.rs#t@1', kind: 'test', name: 't', path: 't.rs', crate_name: 'a', provenance: 'coverage' },
      ],
      edges: [{ from: 'test:t.rs#t@1', to: 'crate:a', kind: 'tests', provenance: 'coverage' }],
      reverse_edges: [{ from: 'crate:a', to: 'test:t.rs#t@1', kind: 'tests', provenance: 'coverage' }],
      stats: {},
      digest: '',
    });
    const path = join(validatorRoot, 'graph.json');
    writeFileSync(path, `${JSON.stringify(base)}\n`);
    const accepted = loadImpactGraph(validatorRoot, { path });
    check('the selector accepts a Rust-validated graph', accepted.error === null, accepted.error ?? '');
    const mutations = [];
    const digest = structuredClone(base);
    digest.nodes[0].name = 'tampered';
    mutations.push(['invalid digest', digest, 'digest_mismatch']);
    const dangling = structuredClone(base);
    dangling.nodes.pop();
    mutations.push(['dangling edge', dangling, 'dangling_edge']);
    const reverse = structuredClone(base);
    reverse.reverse_edges = [];
    mutations.push(['reverse-edge mismatch', reverse, 'reverse_mismatch']);
    const ordering = structuredClone(base);
    ordering.nodes.reverse();
    mutations.push(['unsorted nodes', ordering, 'unsorted']);
    for (const [label, document, code] of mutations) {
      writeFileSync(path, `${JSON.stringify(document)}\n`);
      const refused = loadImpactGraph(validatorRoot, { path });
      const direct = validateImpactGraph(document, null, { root: validatorRoot });
      check(
        `${label} is refused by the selector through the Rust validator`,
        refused.error !== null && refused.code === code && direct === refused.error,
        JSON.stringify({ refused, direct }),
      );
    }
  } finally {
    rmSync(validatorRoot, { recursive: true, force: true });
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
  const selection = select(files, {
    mode,
    base: arg('base'),
    graphPath: arg('impact-graph'),
  });
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
