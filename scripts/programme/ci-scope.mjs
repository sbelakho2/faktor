#!/usr/bin/env node
// Change-scoped main-branch CI planner.
//
// `.woodpecker/main.yaml` must never guess a Cargo package name from a
// directory spelling (`crates/engineering-core` is package
// `engineering-core`, not `faktor-engineering-core`) and must never silently
// run nothing for an unmappable path. This script is the single mapping
// authority for the main lane:
//
//   * changed files come from an explicit base with rename detection DISABLED
//     (a rename is delete+add, so both paths select) and every diff status;
//   * `crates/**` and `tests/**` paths map to the EXACT workspace package
//     name read from that member's Cargo.toml;
//   * docs/apps/site/programme/mutation/separation/CI trees map to bounded
//     scoped command groups;
//   * an unknown path is a typed refusal (exit 3), never an empty selection;
//   * workspace manifest/toolchain changes select the workspace compile and
//     still test any crate whose source also changed — never a whole-workspace
//     test run;
//   * the written shell files are executed by toolchain-specific CI steps
//     (rust / node / vscode / jetbrains).
//
// Modes:
//   node scripts/programme/ci-scope.mjs --selftest
//   node scripts/programme/ci-scope.mjs --plan [--base <rev>] [--out <file>]
//        [--changed-file <path>]...
//
// Exit codes: 0 plan written; 2 usage; 3 typed scope refusal.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(import.meta.url);
const ROOT = resolve(dirname(SCRIPT), '..', '..');
const DEFAULT_OUT = 'target/certification/ci/plan.json';
const SCRIPT_DIR = 'target/certification/ci';
const MANIFESTS = new Set(['Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml']);

export class ScopeError extends Error {
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'ScopeError';
    this.code = code;
    this.details = details;
  }
}

function read(path) {
  return readFileSync(path, 'utf8');
}

function normalizePath(path) {
  return path.split('\\').join('/').replace(/^\.\//, '').replace(/\/+$/, '');
}

function uniqueSorted(values) {
  return [...new Set(values.filter((value) => value.length > 0))].sort();
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** `[package] name` in one member manifest; null when the manifest is virtual. */
export function packageName(manifest) {
  const start = manifest.search(/^\[package\]\s*$/m);
  if (start === -1) return null;
  const rest = manifest.slice(start + '[package]'.length);
  const next = rest.search(/^\[/m);
  const block = next === -1 ? rest : rest.slice(0, next);
  const name = /^\s*name\s*=\s*"([^"]+)"/m.exec(block);
  return name === null ? null : name[1];
}

/** Workspace member directories from the root manifest, globs expanded. */
export function workspaceMembers(root = ROOT) {
  let text;
  try {
    text = read(join(root, 'Cargo.toml'));
  } catch (error) {
    throw new ScopeError('workspace-manifest', `cannot read Cargo.toml: ${error.message}`);
  }
  const section = /\[workspace\][\s\S]*?members\s*=\s*\[([\s\S]*?)\]/.exec(text);
  if (section === null) return [];
  const members = [];
  for (const match of section[1].matchAll(/"([^"]+)"/g)) {
    const pattern = normalizePath(match[1]);
    if (!pattern.includes('*')) {
      members.push(pattern);
      continue;
    }
    const base = pattern.slice(0, pattern.indexOf('*')).replace(/\/$/, '');
    const dir = join(root, base);
    if (!existsSync(dir)) continue;
    const regex = new RegExp(`^${pattern.split('*').map(escapeRegExp).join('.*')}$`);
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const candidate = `${base}/${entry.name}`;
      if (regex.test(candidate)) members.push(candidate);
    }
  }
  return uniqueSorted(members);
}

/**
 * Exact workspace package index. The directory is the selection key; the
 * package name is what `cargo -p` receives, so a directory/package mismatch
 * can never turn into an invented name.
 */
export function workspacePackages(root = ROOT) {
  const packages = [];
  for (const dir of workspaceMembers(root)) {
    let manifest;
    try {
      manifest = read(join(root, dir, 'Cargo.toml'));
    } catch (error) {
      throw new ScopeError('workspace-manifest', `cannot read ${dir}/Cargo.toml: ${error.message}`);
    }
    const name = packageName(manifest);
    if (name === null) {
      throw new ScopeError('workspace-manifest', `${dir}/Cargo.toml has no [package] name`);
    }
    packages.push({ dir, name });
  }
  packages.sort((a, b) => b.dir.length - a.dir.length || a.dir.localeCompare(b.dir));
  return packages;
}

export function crateForPath(path, packages) {
  const normalized = normalizePath(path);
  for (const pkg of packages) {
    if (normalized === pkg.dir || normalized.startsWith(`${pkg.dir}/`)) return pkg;
  }
  return null;
}

// Ordered: a more specific tree must precede its generic parent (`docs/`).
const SUITE_PREFIXES = Object.freeze([
  ['tests/test-separation/', 'separation'],
  ['scripts/check-test-separation.mjs', 'separation'],
  ['scripts/mutations/', 'mutation'],
  ['scripts/mutation-run.mjs', 'mutation'],
  ['scripts/mutation-support.mjs', 'mutation'],
  ['scripts/check-mutation-surfaces.mjs', 'mutation'],
  ['scripts/check-invariants.mjs', 'mutation'],
  ['scripts/check-invariant-coverage.mjs', 'mutation'],
  ['tests/invariants.toml', 'mutation'],
  ['tests/mutation-surfaces.json', 'mutation'],
  ['tests/invariant-coverage.json', 'mutation'],
  ['tests/programme/', 'programme'],
  ['scripts/programme/', 'programme'],
  ['tests/lanes.toml', 'programme'],
  ['tests/edition5/', 'edition5'],
  ['scripts/edition5/', 'edition5'],
  ['tests/edition8/', 'edition8'],
  ['scripts/edition8/', 'edition8'],
  ['docs/specs/edition5/', 'edition5'],
  ['docs/specs/edition8/', 'edition8'],
  ['.woodpecker/', 'ci'],
  ['docker/', 'ci'],
  ['scripts/woodpecker/', 'ci'],
  ['apps/vscode/', 'vscode'],
  ['apps/jetbrains/', 'jetbrains'],
  ['site/', 'site'],
  ['tests/verification/', 'verification'],
  ['docs/contracts/', 'contracts'],
  ['evidence-cas/', 'contracts'],
  ['fixtures/', 'fixtures'],
  ['fuzz/', 'fuzz'],
  ['ui/', 'branding'],
  ['scripts/', 'scripts'],
  ['docs/', 'docs'],
  ['README.md', 'docs'],
  ['AGENTS.md', 'docs'],
  ['LICENSE', 'docs'],
  ['NOTICE', 'docs'],
  ['Report.pdf', 'docs'],
]);

export function suiteForPath(path) {
  const normalized = normalizePath(path);
  for (const [prefix, suite] of SUITE_PREFIXES) {
    if (normalized === prefix || normalized.startsWith(prefix)) return suite;
  }
  return null;
}

const SUITE_COMMANDS = Object.freeze({
  programme: [
    'node scripts/programme/build-registry.mjs --check',
    'node scripts/programme/check-registry.mjs --selftest',
    'node scripts/programme/check-registry.mjs --check --no-probe',
    'node scripts/programme/select-tests.mjs selftest',
    'node scripts/programme/check-core-dumps.mjs --selftest',
    'node scripts/programme/check-core-dumps.mjs',
    'node scripts/programme/check-settlement.mjs --selftest',
    'node scripts/programme/check-settlement.mjs',
  ],
  mutation: [
    'node scripts/check-invariants.mjs',
    'node scripts/check-mutation-surfaces.mjs --selftest',
    'node scripts/check-mutation-surfaces.mjs --check',
    'node scripts/mutation-run.mjs --check-specs',
    'node scripts/check-test-separation.mjs --selftest',
    'node scripts/check-test-separation.mjs --check',
  ],
  separation: [
    'node scripts/check-test-separation.mjs --selftest',
    'node scripts/check-test-separation.mjs --check',
    'node scripts/mutation-run.mjs --check-specs',
  ],
  docs: ['bash scripts/check-docs-sync.sh'],
  ci: [
    'node scripts/certification/check-workflow-graph.mjs selftest',
    'node scripts/certification/check-workflow-graph.mjs --check',
    'sh scripts/check-ci-image-pins.sh --selftest',
    'sh scripts/check-ci-image-pins.sh',
  ],
  edition5: ['node scripts/edition5/run.mjs --skip-jetbrains --no-verify'],
  edition8: ['node tests/edition8/registry.mjs'],
  contracts: ['node scripts/protocol-codegen.mjs --check-clients'],
  branding: ['bash scripts/branding-scan.sh'],
  scripts: ['node scripts/dogfood.mjs --coverage'],
  verification: ['node tests/verification/webview_xss.mjs'],
  site: ['bash scripts/site-zero-js.sh'],
});

function rustSuiteCommands(suite) {
  switch (suite) {
    case 'fixtures':
      return ['cargo test -p faktor-tests-integration --all-features'];
    case 'fuzz':
      return ['cargo test -p faktor-tests-fuzz-seeds --all-features'];
    default:
      return [];
  }
}

export const TOOLCHAIN_SUITES = Object.freeze({
  vscode: [
    'npm --prefix apps/vscode ci --no-audit --no-fund',
    'npm --prefix apps/vscode run build',
    'node apps/vscode/scripts/selftest.mjs',
  ],
  jetbrains: [
    'bash scripts/check-gradle-integrity.sh',
    'bash apps/jetbrains/compile-and-smoke.sh',
  ],
});

function shellScript(commands) {
  return `#!/bin/sh\n# Generated by scripts/programme/ci-scope.mjs; do not edit.\nset -eu\n${
    commands.length > 0 ? commands.join('\n') : ': # no work in this scope'
  }\n`;
}

/** Build one plan. Throws ScopeError for every refusal; never returns empty work. */
export function buildPlan(changedPaths, { packages = workspacePackages(), root = ROOT } = {}) {
  const changed = uniqueSorted(
    changedPaths.map(normalizePath).filter((path) => path.length > 0),
  );
  if (changed.length === 0) {
    throw new ScopeError(
      'empty-diff',
      'the base produced no changed paths; refusing to silently test nothing',
    );
  }
  const crates = new Set();
  const suites = new Set();
  const unmapped = [];
  const removed = new Set();
  const notes = [];
  const workspace = changed.some((path) => MANIFESTS.has(path));

  for (const path of changed) {
    if (MANIFESTS.has(path)) continue;
    const crate = crateForPath(path, packages);
    if (crate !== null) {
      crates.add(crate.name);
      continue;
    }
    const suite = suiteForPath(path);
    if (suite !== null) {
      suites.add(suite);
      continue;
    }
    const top = path.split('/').slice(0, 2).join('/');
    if (
      workspace &&
      (path.startsWith('crates/') || path.startsWith('tests/')) &&
      top.split('/').length === 2 &&
      !existsSync(join(root, top))
    ) {
      removed.add(top);
      notes.push(`${top}: removed crate/test directory (workspace manifest change compiles the workspace)`);
      continue;
    }
    unmapped.push(path);
  }

  if (unmapped.length > 0) {
    throw new ScopeError(
      'unmapped-path',
      `no crate or scoped suite owns: ${unmapped.join(', ')}`,
      { unmapped },
    );
  }
  if (crates.size === 0 && suites.size === 0 && !workspace) {
    throw new ScopeError('no-scope', 'the change maps to no crate, suite or workspace gate');
  }

  const selectedCrates = uniqueSorted([...crates]);
  const selectedSuites = uniqueSorted([...suites]);
  const rust = [];
  if (workspace || selectedCrates.length > 0) {
    rust.push('cargo check --workspace --all-targets');
  }
  for (const name of selectedCrates) {
    rust.push(`cargo fmt --check -p ${name}`);
    rust.push(`cargo clippy -p ${name} --all-targets -- -D warnings`);
    rust.push(`cargo test -p ${name}`);
  }
  const node = [];
  const vscode = [];
  const jetbrains = [];
  for (const suite of selectedSuites) {
    if (Object.hasOwn(TOOLCHAIN_SUITES, suite)) {
      (suite === 'vscode' ? vscode : jetbrains).push(...TOOLCHAIN_SUITES[suite]);
      continue;
    }
    node.push(...(SUITE_COMMANDS[suite] ?? []));
    rust.push(...rustSuiteCommands(suite));
  }
  if (workspace && selectedCrates.length === 0) {
    notes.push('workspace manifest/toolchain change: workspace compile only, no crate source changed');
  }

  return {
    schema: 'faktor-ci-scope/v1',
    root,
    changed,
    crates: selectedCrates,
    removed_crates: uniqueSorted([...removed]),
    suites: selectedSuites,
    workspace,
    rust,
    node,
    vscode,
    jetbrains,
    notes,
  };
}

function git(root, args, options = {}) {
  return execFileSync('git', ['-C', root, ...args], {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
    ...options,
  });
}

function revisionExists(root, revision) {
  try {
    git(root, ['rev-parse', '--verify', '--quiet', `${revision}^{commit}`], { stdio: ['ignore', 'ignore', 'ignore'] });
    return true;
  } catch {
    return false;
  }
}

const REVISION = /^[A-Za-z0-9][A-Za-z0-9._/~^-]*$/;

/**
 * Resolve the diff base. Empty/all-zero means the parent; a temporarily
 * unfetched SHA is fetched once; if it still does not exist the parent is the
 * documented fallback. No parent means a typed refusal.
 */
export function resolveBase(root, requested = '', { fetch = true } = {}) {
  let candidate = String(requested ?? '').trim();
  if (candidate === '' || /^0+$/.test(candidate)) candidate = 'HEAD~1';
  if (!REVISION.test(candidate)) {
    throw new ScopeError('base-invalid', `invalid base revision ${JSON.stringify(candidate)}`);
  }
  if (!revisionExists(root, candidate) && fetch) {
    try {
      git(root, ['fetch', '--no-tags', '--depth=2', 'origin', candidate], {
        stdio: ['ignore', 'ignore', 'ignore'],
        timeout: 60_000,
      });
    } catch {
      // Offline / shallow fallback below.
    }
  }
  if (revisionExists(root, candidate)) {
    return { base: candidate, requested, fallback: false };
  }
  if (revisionExists(root, 'HEAD~1')) {
    return { base: 'HEAD~1', requested, fallback: true };
  }
  throw new ScopeError(
    'base-unavailable',
    `base ${JSON.stringify(candidate)} does not exist and HEAD has no parent; cannot derive a change scope`,
  );
}

/** NUL-delimited names, no rename detection, every diff status. */
export function changedFiles(root, base) {
  if (!revisionExists(root, base)) {
    throw new ScopeError('base-unavailable', `base ${JSON.stringify(base)} is not a commit`);
  }
  const attempts = base === 'HEAD~1' ? [base] : [base, 'HEAD~1'];
  let lastError = null;
  for (const attempt of attempts) {
    try {
      const output = git(root, [
        'diff',
        '--name-only',
        '--no-renames',
        '--diff-filter=ACMRD',
        '-z',
        attempt,
        'HEAD',
      ]);
      return uniqueSorted(output.split('\0').map(normalizePath));
    } catch (error) {
      lastError = error;
    }
  }
  throw new ScopeError(
    'diff-failed',
    `cannot diff ${JSON.stringify(base)}..HEAD: ${lastError?.message ?? lastError}`,
  );
}

function writeOutputs(root, plan, outPath) {
  const out = resolve(root, outPath);
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, `${JSON.stringify(plan, null, 2)}\n`);
  const dir = join(root, SCRIPT_DIR);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'scope-rust.sh'), shellScript(plan.rust), { mode: 0o755 });
  writeFileSync(join(dir, 'scope-node.sh'), shellScript(plan.node), { mode: 0o755 });
  writeFileSync(join(dir, 'scope-vscode.sh'), shellScript(plan.vscode), { mode: 0o755 });
  writeFileSync(join(dir, 'scope-jetbrains.sh'), shellScript(plan.jetbrains), { mode: 0o755 });
  return out;
}

function planCommand(args) {
  const root = resolve(argValue(args, '--root', ROOT));
  const out = argValue(args, '--out', DEFAULT_OUT);
  const explicit = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--changed-file' && index + 1 < args.length) explicit.push(args[index + 1]);
  }
  const requestedBase = argValue(args, '--base', process.env.CI_COMMIT_BEFORE_SHA ?? '');
  let base = null;
  let fallback = false;
  let changed;
  if (explicit.length > 0) {
    changed = uniqueSorted(explicit.map(normalizePath));
  } else {
    const resolved = resolveBase(root, requestedBase);
    base = resolved.base;
    fallback = resolved.fallback;
    changed = changedFiles(root, resolved.base);
  }
  const plan = buildPlan(changed, { root });
  plan.base = base;
  plan.requested_base = requestedBase;
  plan.base_fallback = fallback;
  const written = writeOutputs(root, plan, out);
  console.log(
    `ci-scope: base=${base ?? '<explicit>'}${fallback ? ' (fallback)' : ''} changed=${plan.changed.length} ` +
      `crates=[${plan.crates.join(',')}] suites=[${plan.suites.join(',')}] workspace=${plan.workspace}`,
  );
  console.log(
    `ci-scope: rust=${plan.rust.length} node=${plan.node.length} vscode=${plan.vscode.length} ` +
      `jetbrains=${plan.jetbrains.length} -> ${out} (${written})`,
  );
  for (const note of plan.notes) console.log(`ci-scope: note: ${note}`);
  return 0;
}

function argValue(args, name, fallback = '') {
  const index = args.indexOf(name);
  if (index === -1) return fallback;
  const value = args[index + 1];
  if (value === undefined || value.startsWith('--')) return fallback;
  return value;
}

function createWorkspaceFixture() {
  const root = mkdtempSync(join(tmpdir(), 'faktor-ci-scope-workspace-'));
  mkdirSync(join(root, 'crates/store/src'), { recursive: true });
  mkdirSync(join(root, 'crates/engineering-core/src'), { recursive: true });
  mkdirSync(join(root, 'tests/integration/src'), { recursive: true });
  mkdirSync(join(root, 'docs'), { recursive: true });
  mkdirSync(join(root, 'apps/vscode'), { recursive: true });
  mkdirSync(join(root, 'apps/jetbrains'), { recursive: true });
  mkdirSync(join(root, 'site/content'), { recursive: true });
  mkdirSync(join(root, 'scripts/programme'), { recursive: true });
  mkdirSync(join(root, 'tests/programme'), { recursive: true });
  mkdirSync(join(root, 'tests/test-separation/fixtures'), { recursive: true });
  writeFileSync(
    join(root, 'Cargo.toml'),
    '[workspace]\nmembers = ["crates/store", "crates/engineering-core", "tests/integration"]\n',
  );
  writeFileSync(join(root, 'crates/store/Cargo.toml'), '[package]\nname = "faktor-store"\nversion = "0.1.0"\n');
  writeFileSync(
    join(root, 'crates/engineering-core/Cargo.toml'),
    '[package]\nname = "engineering-core"\nversion = "0.1.0"\n',
  );
  writeFileSync(
    join(root, 'tests/integration/Cargo.toml'),
    '[package]\nname = "faktor-tests-integration"\nversion = "0.1.0"\n',
  );
  for (const file of [
    'crates/store/src/lib.rs',
    'crates/engineering-core/src/lib.rs',
    'tests/integration/src/lib.rs',
    'docs/architecture.md',
    'apps/vscode/src/chat.ts',
    'apps/jetbrains/build.gradle.kts',
    'site/content/_index.md',
    'scripts/programme/selector.mjs',
    'tests/programme/selector.mjs',
    'tests/test-separation/fixtures/clean.rs',
  ]) {
    mkdirSync(dirname(join(root, file)), { recursive: true });
    writeFileSync(join(root, file), '// fixture\n');
  }
  const git = (...args) =>
    execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  git('init', '-q');
  git('config', 'user.email', 'ci-scope@example.invalid');
  git('config', 'user.name', 'ci-scope');
  git('add', '.');
  git('commit', '-qm', 'fixture base');
  return root;
}

function createHistoryFixture() {
  const root = createWorkspaceFixture();
  const git = (...args) =>
    execFileSync('git', ['-C', root, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
  writeFileSync(join(root, 'crates/store/src/old.rs'), 'old\n');
  writeFileSync(join(root, 'docs/old.md'), 'old docs\n');
  git('add', '.');
  git('commit', '-qm', 'add rename sources');
  const base = git('rev-parse', 'HEAD').trim();
  git('mv', 'crates/store/src/old.rs', 'crates/store/src/new.rs');
  git('mv', 'docs/old.md', 'docs/new.md');
  rmSync(join(root, 'crates/store/src/lib.rs'));
  writeFileSync(join(root, 'crates/engineering-core/src/lib.rs'), '// changed\n');
  git('add', '-A');
  git('commit', '-qm', 'rename and delete');
  return { root, base, git };
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) {
      console.log(`ci-scope selftest ok: ${name}`);
    } else {
      failures += 1;
      console.error(`ci-scope selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const refusal = (fn) => {
    try {
      fn();
      return null;
    } catch (error) {
      return error instanceof ScopeError ? error : null;
    }
  };

  const fixture = createWorkspaceFixture();
  const history = createHistoryFixture();
  try {
    const packages = workspacePackages(fixture);
    check(
      'workspace package names come from manifests, not directory spelling',
      packages.some((pkg) => pkg.dir === 'crates/engineering-core' && pkg.name === 'engineering-core'),
      packages.map((pkg) => `${pkg.dir}=${pkg.name}`).join(','),
    );
    const engineering = buildPlan(['crates/engineering-core/src/lib.rs'], {
      packages,
      root: fixture,
    });
    check(
      'engineering-core maps to engineering-core (not faktor-engineering-core)',
      engineering.crates.includes('engineering-core') &&
        !engineering.crates.includes('faktor-engineering-core'),
      engineering.crates.join(','),
    );
    check(
      'test crate maps to its exact package name',
      buildPlan(['tests/integration/src/lib.rs'], { packages, root: fixture }).crates.includes(
        'faktor-tests-integration',
      ),
    );
    const docOnly = buildPlan(['docs/architecture.md'], { packages, root: fixture });
    check(
      'docs-only changes select the docs gate and no Rust work',
      docOnly.crates.length === 0 &&
        docOnly.rust.length === 0 &&
        docOnly.suites.includes('docs'),
      `${docOnly.crates.join(',')}|${docOnly.rust.join(',')}`,
    );
    check(
      'docs-only changes never run a workspace compile or test',
      !docOnly.rust.some((command) => command.includes('--workspace')),
    );
    const vscode = buildPlan(['apps/vscode/src/chat.ts'], { packages, root: fixture });
    check(
      'VS Code changes select the vscode toolchain only',
      vscode.crates.length === 0 && vscode.suites.includes('vscode') && vscode.vscode.length === 3,
    );
    const jetbrains = buildPlan(['apps/jetbrains/build.gradle.kts'], { packages, root: fixture });
    check(
      'JetBrains changes select the jetbrains toolchain only',
      jetbrains.crates.length === 0 && jetbrains.suites.includes('jetbrains') && jetbrains.jetbrains.length === 2,
    );
    const site = buildPlan(['site/content/_index.md'], { packages, root: fixture });
    check(
      'website changes select the scoped site gate',
      site.crates.length === 0 && site.suites.includes('site') && site.node.includes('bash scripts/site-zero-js.sh'),
    );
    const programme = buildPlan(['tests/programme/selector.mjs'], { packages, root: fixture });
    check(
      'programme changes select programme checks only',
      programme.crates.length === 0 &&
        programme.suites.includes('programme') &&
        programme.node.some((command) => command.includes('check-registry.mjs')),
    );
    const separation = buildPlan(['tests/test-separation/fixtures/clean.rs'], {
      packages,
      root: fixture,
    });
    check(
      'test-separation changes select the separation checker',
      separation.suites.includes('separation') &&
        separation.node.includes('node scripts/check-test-separation.mjs --check'),
    );
    const mutation = buildPlan(['tests/invariants.toml'], { packages, root: fixture });
    check(
      'invariant/mutation changes select the mutation checks',
      mutation.suites.includes('mutation') &&
        mutation.node.includes('node scripts/check-mutation-surfaces.mjs --check') &&
        mutation.node.includes('node scripts/mutation-run.mjs --check-specs'),
    );
    const oneCrate = buildPlan(['crates/store/src/lib.rs'], { packages, root: fixture });
    check(
      'a crate plan tests only that crate and never the workspace',
      oneCrate.rust.includes('cargo test -p faktor-store') &&
        oneCrate.rust.filter((command) => command.startsWith('cargo test')).every(
          (command) => command.includes('-p faktor-store') && !command.includes('--workspace'),
        ),
    );
    const manifest = buildPlan(['Cargo.toml'], { packages, root: fixture });
    check(
      'a manifest-only change compiles the workspace and runs no whole-workspace tests',
      manifest.workspace &&
        manifest.rust.length === 1 &&
        manifest.rust[0] === 'cargo check --workspace --all-targets' &&
        !manifest.rust.some((command) => command.startsWith('cargo test')),
    );
    const manifestPlusSource = buildPlan(
      ['Cargo.lock', 'crates/store/src/lib.rs'],
      { packages, root: fixture },
    );
    check(
      'a manifest change still tests the crate whose source changed',
      manifestPlusSource.workspace && manifestPlusSource.rust.includes('cargo test -p faktor-store'),
    );

    const cratesRoot = refusal(() =>
      buildPlan(['crates/ghost/src/lib.rs'], { packages, root: fixture }),
    );
    check(
      'an unmappable crates/ path is a typed refusal',
      cratesRoot !== null && cratesRoot.code === 'unmapped-path',
      JSON.stringify(cratesRoot),
    );
    const unknownRoot = refusal(() => buildPlan(['mystery.bin'], { packages, root: fixture }));
    check(
      'an unmapped root file is a typed refusal',
      unknownRoot !== null && unknownRoot.code === 'unmapped-path',
    );
    const empty = refusal(() => buildPlan([], { packages, root: fixture }));
    check('an empty diff is a typed refusal', empty !== null && empty.code === 'empty-diff');

    const removed = buildPlan(['Cargo.toml', 'crates/ghost/src/lib.rs'], {
      packages,
      root: fixture,
    });
    check(
      'a removed crate path is absorbed by the workspace manifest change',
      removed.workspace && removed.removed_crates.includes('crates/ghost'),
    );

    const head = history.git('rev-parse', 'HEAD').trim();
    const emptyBase = resolveBase(history.root, '', { fetch: false });
    check(
      'empty base falls back to the parent commit',
      emptyBase.base === 'HEAD~1' && emptyBase.fallback === false,
      JSON.stringify(emptyBase),
    );
    const zeroBase = resolveBase(history.root, '0'.repeat(40), { fetch: false });
    check(
      'an all-zero base falls back to the parent commit',
      zeroBase.base === 'HEAD~1' && zeroBase.fallback === false,
      JSON.stringify(zeroBase),
    );
    const missing = resolveBase(history.root, 'deadbeefdeadbeefdeadbeefdeadbeefdeadbeef', {
      fetch: false,
    });
    check(
      'an unfetched base falls back to the parent commit',
      missing.base === 'HEAD~1' && missing.fallback === true,
      JSON.stringify(missing),
    );
    const explicit = resolveBase(history.root, history.base, { fetch: false });
    check('an existing explicit base is honored', explicit.base === history.base && !explicit.fallback);
    const badRevision = refusal(() => resolveBase(history.root, '--upload-pack=evil', { fetch: false }));
    check(
      'an option-like base is a typed refusal',
      badRevision !== null && badRevision.code === 'base-invalid',
    );
    const files = changedFiles(history.root, history.base);
    check(
      'deleted and renamed paths are both present (no-renames diff)',
      files.includes('crates/store/src/new.rs') &&
        files.includes('docs/new.md') &&
        files.includes('docs/old.md'),
      files.join(','),
    );
    const historyPlan = buildPlan(files, { root: history.root });
    check(
      'a rename/delete commit still maps to the owning crates and suites',
      historyPlan.crates.includes('faktor-store') &&
        historyPlan.crates.includes('engineering-core') &&
        historyPlan.suites.includes('docs'),
      JSON.stringify(historyPlan.crates),
    );

    const single = mkdtempSync(join(tmpdir(), 'faktor-ci-scope-single-'));
    try {
      const git = (...args) =>
        execFileSync('git', ['-C', single, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] });
      git('init', '-q');
      git('config', 'user.email', 'ci-scope@example.invalid');
      git('config', 'user.name', 'ci-scope');
      writeFileSync(join(single, 'README.md'), 'one commit\n');
      git('add', '.');
      git('commit', '-qm', 'only commit');
      const noParent = refusal(() => resolveBase(single, '', { fetch: false }));
      check(
        'a root commit with no parent is a typed refusal, never an empty scope',
        noParent !== null && noParent.code === 'base-unavailable',
      );
    } finally {
      rmSync(single, { recursive: true, force: true });
    }
  } finally {
    rmSync(fixture, { recursive: true, force: true });
    rmSync(history.root, { recursive: true, force: true });
  }

  const realPlan = buildPlan(['crates/engineering-core/src/lib.rs']);
  check(
    'the committed workspace maps engineering-core to its real package name',
    realPlan.crates.includes('engineering-core') &&
      !realPlan.crates.includes('faktor-engineering-core'),
    realPlan.crates.join(','),
  );
  check(
    'the committed workspace emits no whole-workspace test command',
    realPlan.rust.filter((command) => command.startsWith('cargo test')).every(
      (command) => !command.includes('--workspace'),
    ),
  );

  if (failures > 0) {
    console.error(`ci-scope selftest: FAIL (${failures} failure(s))`);
    return 1;
  }
  console.log('ci-scope selftest: PASS');
  return 0;
}

function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) return selftest();
  if (args.includes('--plan')) {
    try {
      return planCommand(args);
    } catch (error) {
      if (error instanceof ScopeError) {
        console.error(`ci-scope: ${error.code}: ${error.message}`);
        return 3;
      }
      console.error(`ci-scope: ${error?.stack ?? error}`);
      return 2;
    }
  }
  console.error(
    'usage: node scripts/programme/ci-scope.mjs --selftest | --plan [--base REV] [--out FILE] [--changed-file PATH]...',
  );
  return 2;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(SCRIPT)) {
  process.exitCode = main();
}
