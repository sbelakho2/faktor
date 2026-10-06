#!/usr/bin/env node
// Invariant registry coverage gate (audit release-program item).
//
// `tests/invariants.toml` is the registry of high-criticality invariants. This
// gate makes the registry honest instead of declarative:
//
//   1. every invariant's named Rust test fns must exist EXACTLY ONCE across
//      `crates/**` and `tests/**` (`fn <name>` occurrences);
//   2. every invariant must carry a `mutation_witness` test fn (a test that
//      plants a deliberate violation and proves the oracle fires) or a
//      non-empty `mutation_debt` reason — never both, never neither;
//   3. criticality: every invariant is release-critical by default because
//      its `authority` names a production gate. A release-critical invariant
//      MUST ship an executable `mutation_command`; a non-empty
//      `mutation_debt` on it is a hard failure in BOTH modes. An entry may be
//      explicitly downgraded with `release_critical = false`, which requires
//      a non-empty `criticality_reason`; only then may `mutation_debt`
//      remain, reported as a warning (never silently);
//   4. `script_check` entries (non-Rust gates) must resolve to a literal
//      VS Code selftest step label (`apps/vscode/scripts/selftest.mjs`) or to
//      a command line wired in a `.woodpecker` workflow;
//   5. ids are unique `INV-...` and `platforms` is non-empty.
//
// Usage:
//   node scripts/check-invariants.mjs [--root DIR] [--registry FILE]
//   node scripts/check-invariants.mjs --mutations
//   node scripts/check-invariants.mjs --root FIXTURE --registry FIXTURE.toml --expect-fail CODE
//
// Exit codes: 0 pass (or expected failure matched); 1 violations; 2 usage.
//
// `--mutations` additionally executes every release-critical invariant's
// `mutation_command` and requires the planted violation to be detected; a
// release-critical entry without one already fails before this mode runs.

import { createHash } from 'node:crypto';
import { provisionSupport } from './mutation-support.mjs';
import { cpSync, existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { execSync, spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const ISOLATION_EXCLUDES = new Set(['target', '.git', 'node_modules', '.idea']);
const PLATFORMS = new Set(['linux', 'macos', 'windows']);
const ID_RE = /^INV-[A-Z0-9]+(?:-[A-Z0-9]+)*$/;
const FN_RE =
  /^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:const|async|unsafe)\s+)*fn\s+([A-Za-z0-9_]+)/;
const WORKFLOWS = [
  '.woodpecker/trusted/trusted.yaml',
  '.woodpecker/trusted/nightly.yaml',
  '.woodpecker/untrusted/pr.yaml',
];

function argValue(args, name, fallback = '') {
  const idx = args.indexOf(name);
  if (idx === -1) return fallback;
  const value = args[idx + 1];
  if (value === undefined || value.startsWith('--')) return '';
  return value;
}

// ---------------------------------------------- campaign isolation + barrier
//
// P0-CERT: the mutation campaign must never edit the trusted source tree,
// even transiently. Every planted edit and gate runs in an isolated copy
// (FAKTOR_MUTATION_ROOT) with a dedicated CARGO_TARGET_DIR, while this
// process continuously hashes the REAL checkout and fails if a single byte
// changes during the campaign.

function excludedUnder(root, path) {
  const rel = relative(root, path);
  if (rel === '') return false;
  return rel.split(/[\\/]/).some((part) => ISOLATION_EXCLUDES.has(part));
}

function listTreeFiles(root) {
  const files = [];
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const path = join(dir, name);
      const rel = relative(root, path);
      if (rel.split(sep).some((part) => ISOLATION_EXCLUDES.has(part))) continue;
      const stat = lstatSync(path);
      if (stat.isDirectory()) walk(path);
      else if (stat.isFile()) files.push(path);
    }
  };
  walk(root);
  files.sort();
  return files;
}

/** Content digest of every source file (build/VCS/dependency trees excluded). */
function treeContentDigest(root) {
  const hash = createHash('sha256');
  for (const file of listTreeFiles(root)) {
    hash.update(relative(root, file).split(sep).join('/'));
    hash.update('\0');
    hash.update(readFileSync(file));
    hash.update('\0');
  }
  return hash.digest('hex');
}

/** Cheap continuous digest: path + size + mtime, no file contents. */
function treeMetaDigest(root) {
  const hash = createHash('sha256');
  for (const file of listTreeFiles(root)) {
    const stat = statSync(file);
    hash.update(relative(root, file).split(sep).join('/'));
    hash.update(`:${stat.size}:${stat.mtimeMs}`);
    hash.update('\0');
  }
  return hash.digest('hex');
}

/** Null when the tracked tree is clean; else the failing git command. */
function gitCleanProblem(root) {
  for (const command of ['git diff --exit-code --quiet', 'git diff --cached --exit-code --quiet']) {
    try {
      execSync(command, { cwd: root, stdio: 'pipe' });
    } catch {
      return command;
    }
  }
  return null;
}

function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/**
 * Continuous barrier child: samples the trusted checkout's METADATA every
 * 200ms (path+size+mtime; any accidental write bumps mtime, even if the
 * content is restored) and, when the metadata moved, re-digests the CONTENT
 * to distinguish a real byte change from metadata-only churn. It reports
 * both counts as JSON on exit; the parent fails closed on either. A
 * metadata-preserving content rewrite (utimensat) is outside this sampler's
 * contract: CI runs the campaign in a scratch copy, leaving the trusted
 * checkout untouched, and the before/after content digests bound the window.
 */
function treeBarrier(args) {
  const root = resolve(args[1]);
  const baseline = args[2];
  const baselineContent = args[3];
  const out = args[4];
  let violations = 0;
  let contentViolations = 0;
  const timer = setInterval(() => {
    try {
      if (treeMetaDigest(root) !== baseline) {
        violations += 1;
        if (contentViolations === 0 && treeContentDigest(root) !== baselineContent) {
          contentViolations += 1;
        }
      }
    } catch {
      violations += 1;
      contentViolations += 1;
    }
  }, 200);
  const finish = () => {
    clearInterval(timer);
    try {
      writeFileSync(out, JSON.stringify({ violations, content_violations: contentViolations }));
    } catch {
      // best effort: the parent fails closed when the file is absent
    }
    process.exit(0);
  };
  for (const signal of ['SIGTERM', 'SIGINT', 'SIGHUP']) {
    process.on(signal, finish);
  }
}

// ------------------------------------------------------------------ parsing

function unquote(text, line, problems) {
  if (!text.startsWith('"')) {
    problems.push({ line, message: `expected a quoted string, got '${text}'` });
    return null;
  }
  let out = '';
  let i = 1;
  while (i < text.length) {
    const ch = text[i];
    if (ch === '\\') {
      const next = text[i + 1];
      if (next === '"' || next === '\\') {
        out += next;
        i += 2;
        continue;
      }
      problems.push({ line, message: `unsupported escape '\\${next}'` });
      return null;
    }
    if (ch === '"') {
      const rest = text.slice(i + 1).trim();
      if (rest !== '' && !rest.startsWith('#')) {
        problems.push({ line, message: `unexpected text after string: '${rest}'` });
        return null;
      }
      return out;
    }
    out += ch;
    i += 1;
  }
  problems.push({ line, message: 'unterminated string' });
  return null;
}

function parseArray(text, line, problems) {
  if (!text.startsWith('[')) return null;
  const values = [];
  let i = 1;
  for (;;) {
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (i >= text.length) {
      problems.push({ line, message: 'unterminated array' });
      return null;
    }
    if (text[i] === ']') {
      const rest = text.slice(i + 1).trim();
      if (rest !== '' && !rest.startsWith('#')) {
        problems.push({ line, message: `unexpected text after array: '${rest}'` });
        return null;
      }
      return values;
    }
    if (text[i] !== '"') {
      problems.push({ line, message: `array items must be quoted strings: '${text.slice(i)}'` });
      return null;
    }
    let end = i + 1;
    let item = '';
    while (end < text.length) {
      if (text[end] === '\\') {
        item += text[end + 1] === undefined ? '' : text[end + 1];
        end += 2;
        continue;
      }
      if (text[end] === '"') break;
      item += text[end];
      end += 1;
    }
    if (end >= text.length) {
      problems.push({ line, message: 'unterminated array string' });
      return null;
    }
    values.push(item);
    i = end + 1;
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] === ',') {
      i += 1;
      continue;
    }
    if (text[i] === ']') continue;
    problems.push({ line, message: `expected ',' or ']' in array, got '${text.slice(i)}'` });
    return null;
  }
}

function parseRegistry(text) {
  const entries = [];
  const problems = [];
  let current = null;
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim();
    if (line === '' || line.startsWith('#')) continue;
    if (line === '[[invariant]]') {
      current = { __line: i + 1, __keys: new Set() };
      entries.push(current);
      continue;
    }
    if (line.startsWith('[')) {
      problems.push({ line: i + 1, message: `unsupported table header '${line}'` });
      current = null;
      continue;
    }
    if (current === null) {
      problems.push({ line: i + 1, message: `key/value outside an [[invariant]] table: '${line}'` });
      continue;
    }
    const match = /^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$/.exec(line);
    if (!match) {
      problems.push({ line: i + 1, message: `malformed assignment '${line}'` });
      continue;
    }
    const key = match[1];
    const rest = match[2].trim();
    if (current.__keys.has(key)) {
      problems.push({ line: i + 1, message: `duplicate key '${key}'` });
      continue;
    }
    current.__keys.add(key);
    if (rest.startsWith('"')) {
      current[key] = unquote(rest, i + 1, problems);
    } else if (rest.startsWith('[')) {
      current[key] = parseArray(rest, i + 1, problems);
    } else if (rest === 'true' || rest === 'false') {
      current[key] = rest === 'true';
    } else {
      problems.push({ line: i + 1, message: `unsupported value '${rest}' for '${key}'` });
    }
  }
  return { entries, problems };
}

// ------------------------------------------------------------------ indexes

function walkRs(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === 'node_modules' || entry.name.startsWith('.')) {
      continue;
    }
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      walkRs(full, out);
    } else if (entry.isFile() && entry.name.endsWith('.rs')) {
      out.push(full);
    }
  }
  return out;
}

function buildFnIndex(root) {
  const index = new Map();
  for (const top of ['crates', 'tests']) {
    const dir = join(root, top);
    if (!existsSync(dir)) continue;
    for (const file of walkRs(dir)) {
      const rel = relative(root, file).split('\\').join('/');
      const lines = readFileSync(file, 'utf8').split('\n');
      for (let i = 0; i < lines.length; i += 1) {
        const match = FN_RE.exec(lines[i]);
        if (!match) continue;
        const sites = index.get(match[1]) || [];
        sites.push(`${rel}:${i + 1}`);
        index.set(match[1], sites);
      }
    }
  }
  return index;
}

function selftestLabels(root) {
  const file = join(root, 'apps/vscode/scripts/selftest.mjs');
  if (!existsSync(file)) return [];
  const text = readFileSync(file, 'utf8');
  const labels = [];
  for (const quote of ["'", '"']) {
    const marker = `await test(${quote}`;
    let from = 0;
    for (;;) {
      const at = text.indexOf(marker, from);
      if (at === -1) break;
      const start = at + marker.length;
      const end = text.indexOf(quote, start);
      if (end === -1) break;
      labels.push(text.slice(start, end));
      from = end + 1;
    }
  }
  return labels;
}

function kotlinTestFiles(root) {
  const base = join(root, 'apps/jetbrains');
  const files = [];
  const walk = (dir) => {
    if (!existsSync(dir)) return;
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (entry.name.endsWith('.kt')) files.push(full);
    }
  };
  walk(base);
  return files;
}

function jetbrainsStepLabels(root) {
  const labels = [];
  for (const file of kotlinTestFiles(root)) {
    const text = readFileSync(file, 'utf8');
    const marker = 'step(';
    let from = 0;
    for (;;) {
      const at = text.indexOf(marker, from);
      if (at === -1) break;
      let i = at + marker.length;
      while (i < text.length && text[i] === ' ') i += 1;
      const quote = text[i];
      if (quote === '"' || quote === "'") {
        const end = text.indexOf(quote, i + 1);
        if (end !== -1) labels.push(text.slice(i + 1, end));
      }
      from = at + marker.length;
    }
    // Kotlin helper test fns the smokes invoke (`fun assertX(...)`) are
    // executable checks too and may witness an invariant.
    const fnMarker = 'fun ';
    from = 0;
    for (;;) {
      const at = text.indexOf(fnMarker, from);
      if (at === -1) break;
      let i = at + fnMarker.length;
      const start = i;
      while (i < text.length && /[A-Za-z0-9_]/.test(text[i])) i += 1;
      const name = text.slice(start, i);
      if (name.startsWith('assert')) labels.push(name);
      from = at + fnMarker.length;
    }
  }
  return labels;
}

function ciCommands(root) {
  const commands = [];
  for (const rel of WORKFLOWS) {
    const file = join(root, rel);
    if (!existsSync(file)) continue;
    for (const raw of readFileSync(file, 'utf8').split('\n')) {
      const line = raw.trim().replace(/^-\s*/, '');
      if (line === '' || line.startsWith('#')) continue;
      commands.push(line);
    }
  }
  return commands;
}

// ------------------------------------------------------------------- checks

function runCheck({ root, registryPath }) {
  const problems = [];
  const warnings = [];
  const add = (code, detail) => problems.push(`${code}: ${detail}`);

  if (!existsSync(registryPath)) {
    add('missing-registry', `${relative(SCRIPT_ROOT, registryPath)} does not exist`);
    return {
      problems,
      warnings,
      counts: {
        invariants: 0,
        witnesses: 0,
        debts: 0,
        releaseCritical: 0,
        downgraded: 0,
        criticalDebts: 0,
        nonCriticalDebts: 0,
      },
      entries: [],
    };
  }
  const { entries, problems: parseProblems } = parseRegistry(readFileSync(registryPath, 'utf8'));
  for (const problem of parseProblems) {
    add('parse-error', `line ${problem.line}: ${problem.message}`);
  }
  if (parseProblems.length > 0) {
    return {
      problems,
      warnings,
      counts: {
        invariants: entries.length,
        witnesses: 0,
        debts: 0,
        releaseCritical: 0,
        downgraded: 0,
        criticalDebts: 0,
        nonCriticalDebts: 0,
      },
      entries,
    };
  }

  const fnIndex = buildFnIndex(root);
  const labels = [...selftestLabels(root), ...jetbrainsStepLabels(root)];
  const commands = ciCommands(root);
  const hasScriptCheck = (value) =>
    labels.includes(value) || commands.some((command) => command.includes(value));

  const ids = new Set();
  let witnesses = 0;
  let debts = 0;
  let releaseCritical = 0;
  let downgraded = 0;
  let criticalDebts = 0;
  let nonCriticalDebts = 0;

  const requireFn = (invariant, field, value) => {
    const sites = fnIndex.get(value);
    if (!sites) {
      add('missing-test', `${invariant.id} ${field} '${value}' matches no fn in crates/**+tests/**`);
      return;
    }
    if (sites.length > 1) {
      add('duplicate-test', `${invariant.id} ${field} '${value}' is defined ${sites.length} times: ${sites.join(', ')}`);
    }
  };

  for (const entry of entries) {
    const id = typeof entry.id === 'string' ? entry.id : `<line ${entry.__line}>`;
    if (typeof entry.id !== 'string' || !ID_RE.test(entry.id)) {
      add('bad-id', `line ${entry.__line}: missing or malformed id '${entry.id ?? ''}'`);
    } else if (ids.has(entry.id)) {
      add('duplicate-id', `id '${entry.id}' appears twice`);
    } else {
      ids.add(entry.id);
    }
    for (const field of ['statement', 'owner', 'authority']) {
      if (typeof entry[field] !== 'string' || entry[field].trim() === '') {
        add('missing-field', `${id} requires a non-empty '${field}'`);
      }
    }
    if (!Array.isArray(entry.platforms) || entry.platforms.length === 0) {
      add('missing-platforms', `${id} requires a non-empty platforms array`);
    } else {
      for (const platform of entry.platforms) {
        if (!PLATFORMS.has(platform)) {
          add('bad-platform', `${id} platforms entry '${platform}' is not one of linux/macos/windows`);
        }
      }
    }

    const scriptCheck = typeof entry.script_check === 'string' ? entry.script_check.trim() : '';
    if (scriptCheck !== '') {
      if (!hasScriptCheck(scriptCheck)) {
        add(
          'script-check-unwired',
          `${id} script_check '${scriptCheck}' matches no VS Code selftest step, JetBrains smoke step, or .woodpecker command line`,
        );
      }
    } else {
      for (const field of ['unit_test', 'production_wiring']) {
        const value = typeof entry[field] === 'string' ? entry[field].trim() : '';
        if (value === '') {
          add('missing-field', `${id} requires '${field}' (or a script_check)`);
        } else {
          requireFn(entry, field, value);
        }
      }
    }

    // Criticality policy: every invariant is release-critical by default —
    // its `authority` names a production gate, so the registry must prove it
    // with a real planted mutation. Only an explicit downgrade
    // (`release_critical = false`) with a documented reason may keep debt.
    const critical = entry.release_critical !== false;
    const criticalityReason =
      typeof entry.criticality_reason === 'string' ? entry.criticality_reason.trim() : '';
    if (entry.release_critical !== undefined && typeof entry.release_critical !== 'boolean') {
      add('bad-criticality', `${id} release_critical must be a boolean`);
    }
    if (entry.release_critical === false) {
      downgraded += 1;
      if (criticalityReason === '') {
        add(
          'missing-criticality-reason',
          `${id} is downgraded (release_critical = false) without a non-empty criticality_reason`,
        );
      }
    } else {
      releaseCritical += 1;
    }

    const witness = typeof entry.mutation_witness === 'string' ? entry.mutation_witness.trim() : '';
    const debt = typeof entry.mutation_debt === 'string' ? entry.mutation_debt.trim() : '';
    const mutationCommand =
      typeof entry.mutation_command === 'string' ? entry.mutation_command.trim() : '';
    // Proof model: a behavioral witness is evidence, not proof. Every
    // invariant must either ship an executable `mutation_command` that plants
    // the violation and is detected, or carry an explicit mutation_debt; the
    // two are mutually exclusive. A release-critical invariant may NOT carry
    // debt: the checker fails both modes instead of accepting a documented
    // hole in the release gate.
    if (mutationCommand === '' && debt === '') {
      add('missing-mutation-proof', `${id} needs a mutation_command or a mutation_debt reason`);
    }
    if (mutationCommand !== '' && debt !== '') {
      add('mutation-proof-conflict', `${id} sets both mutation_command and mutation_debt`);
    }
    if (witness !== '') {
      // A witness may be a Rust test fn OR a UI/script check label (VS Code
      // selftest step, JetBrains smoke step, or workflow command): a removed
      // control or dropped payload must make a real gate fail.
      if (fnIndex.has(witness)) {
        requireFn(entry, 'mutation_witness', witness);
      } else if (hasScriptCheck(witness)) {
        // wired: the label exists in a real suite
      } else {
        add(
          'missing-test',
          `${id} mutation_witness '${witness}' matches no fn and no script/smoke check`,
        );
      }
      witnesses += 1;
    }
    if (debt !== '') {
      debts += 1;
      if (critical) {
        criticalDebts += 1;
        add(
          'release-critical-mutation-debt',
          `${id} is release-critical but carries mutation_debt ('${debt}'); release-critical invariants must ship an executable mutation_command`,
        );
      } else {
        nonCriticalDebts += 1;
        warnings.push(`${id} (non-critical) carries mutation_debt: ${debt}`);
      }
    }

    if (typeof entry.fault === 'string' && entry.fault.trim() !== '') {
      requireFn(entry, 'fault', entry.fault.trim());
    }
  }

  return {
    problems,
    warnings,
    counts: {
      invariants: entries.length,
      witnesses,
      debts,
      releaseCritical,
      downgraded,
      criticalDebts,
      nonCriticalDebts,
    },
    entries,
  };
}

function main() {
  const args = process.argv.slice(2);
  if (args.includes('-h') || args.includes('--help')) {
    console.log(
      'usage: node scripts/check-invariants.mjs [--root DIR] [--registry FILE] [--expect-fail CODE] [--mutations]',
    );
    return 2;
  }
  const root = resolve(argValue(args, '--root', SCRIPT_ROOT));
  const registryPath = resolve(argValue(args, '--registry', join(root, 'tests/invariants.toml')));
  const mutations = args.includes('--mutations');
  const { problems, warnings, counts, entries } = runCheck({ root, registryPath });
  for (const warning of warnings) console.warn(`invariants-warning: ${warning}`);
  const expectFail = argValue(args, '--expect-fail');
  if (expectFail) {
    if (problems.some((problem) => problem.startsWith(`${expectFail}:`))) {
      console.log(`check-invariants: expected failure '${expectFail}' reproduced (${problems.length} problem(s))`);
      return 0;
    }
    console.error(`check-invariants: expected '${expectFail}:' but got ${JSON.stringify(problems)}`);
    return 1;
  }
  if (problems.length > 0) {
    for (const problem of problems) console.error(`invariants: ${problem}`);
    console.error(
      `check-invariants: FAIL (${problems.length} problem(s) over ${counts.invariants} invariant(s))`,
    );
    return 1;
  }
  if (mutations) {
    // Mutation mode: a witness is only credible when a PLANTED violation
    // makes its gate fail. An invariant either ships a `mutation_command`
    // that plants the violation (and must exit non-zero), or carries an
    // explicit mutation_debt. This closes the audit's
    // "a normal drift check is not a mutation witness" gap.
    //
    // P0-CERT: the campaign may only run from a CLEAN committed checkout
    // (git diff + diff --cached both empty), it runs every planted edit in an
    // isolated copy, and a continuous barrier hashes the real checkout while
    // it runs — any transient edit to the trusted tree fails the campaign.
    const dirtyBefore = gitCleanProblem(root);
    if (dirtyBefore !== null) {
      console.error(
        `check-invariants --mutations: FAIL — the trusted tree is dirty (\`${dirtyBefore}\`); the campaign may only run from the committed checkout`,
      );
      return 1;
    }
    const baselineMeta = treeMetaDigest(root);
    const baselineContent = treeContentDigest(root);
    const barrierDir = mkdtempSync(join(tmpdir(), 'faktor-tree-barrier-'));
    const barrierOut = join(barrierDir, 'violations');
    const hasher = spawn(
      process.execPath,
      [
        fileURLToPath(import.meta.url),
        '--tree-barrier',
        root,
        baselineMeta,
        baselineContent,
        barrierOut,
      ],
      { stdio: 'ignore' },
    );
    const scratch = mkdtempSync(join(tmpdir(), 'faktor-mutation-campaign-'));
    cpSync(root, scratch, { recursive: true, filter: (source) => !excludedUnder(root, source) });
    // Snapshot the gate support trees (P2-9): the CLI is an immutable
    // COPY and node_modules a hardlink snapshot, both content-digested into
    // evidence — never symlinks back into the trusted checkout.
    const support = provisionSupport(root, scratch);
    console.log(
      `invariants-mutations: support cli=${support.cli?.sha256 ?? 'none'} node_modules=${support.node_modules?.digest ?? 'none'} (${support.node_modules?.method ?? 'none'})`,
    );
    try {
      mkdirSync(join(root, 'target', 'certification'), { recursive: true });
      writeFileSync(
        join(root, 'target', 'certification', 'mutation-support.json'),
        `${JSON.stringify(support, null, 2)}\n`,
      );
    } catch {
      // Evidence copy is best-effort; the console line is the record.
    }
    const mutationEnv = {
      ...process.env,
      FAKTOR_MUTATION_ROOT: scratch,
      CARGO_TARGET_DIR: join(root, 'target', 'mutation-campaign'),
    };
    const missing = [];
    const undetected = [];
    let proven = 0;
    try {
      for (const entry of entries) {
        const command = typeof entry.mutation_command === 'string' ? entry.mutation_command.trim() : '';
        if (command === '') {
          if (typeof entry.mutation_debt !== 'string' || entry.mutation_debt.trim() === '') {
            missing.push(entry.id);
          }
          continue;
        }
        // A mutation command exits 0 when its gate DETECTED the planted
        // violation, and non-zero when the mutation slipped through. The
        // command sees the isolated copy through FAKTOR_MUTATION_ROOT.
        try {
          execSync(command, { cwd: root, stdio: 'pipe', env: mutationEnv });
          proven += 1;
        } catch {
          undetected.push(`${entry.id} (${command})`);
        }
      }
    } finally {
      hasher.kill('SIGTERM');
      const deadline = Date.now() + 5000;
      while (!existsSync(barrierOut) && Date.now() < deadline) {
        sleepSync(25);
      }
      rmSync(scratch, { recursive: true, force: true });
    }
    let barrierReport = null;
    if (existsSync(barrierOut)) {
      try {
        barrierReport = JSON.parse(readFileSync(barrierOut, 'utf8'));
      } catch {
        barrierReport = null;
      }
      rmSync(barrierDir, { recursive: true, force: true });
    }
    const contentAfter = treeContentDigest(root);
    const dirtyAfter = gitCleanProblem(root);
    if (
      barrierReport === null ||
      typeof barrierReport.violations !== 'number' ||
      barrierReport.violations > 0 ||
      barrierReport.content_violations > 0
    ) {
      console.error(
        `check-invariants --mutations: FAIL — the tree barrier observed the trusted checkout change during the campaign (metadata samples=${barrierReport?.violations ?? 'unknown'}, content violations=${barrierReport?.content_violations ?? 'unknown'}); mutations must never touch the certifying tree`,
      );
      return 1;
    }
    if (contentAfter !== baselineContent) {
      console.error(
        'check-invariants --mutations: FAIL — the trusted checkout content changed during the campaign',
      );
      return 1;
    }
    if (dirtyAfter !== null) {
      console.error(
        `check-invariants --mutations: FAIL — the trusted tree is dirty after the campaign (\`${dirtyAfter}\`)`,
      );
      return 1;
    }
    if (missing.length > 0 || undetected.length > 0) {
      for (const id of missing) {
        console.error(`invariants-mutations: ${id} has neither a mutation_command nor a mutation_debt`);
      }
      for (const entry of undetected) {
        console.error(
          `invariants-mutations: ${entry} gate did not exit 0 (mutation-run refuses to call an environment/compile/gate failure a detection); the planted violation was NOT proven`,
        );
      }
      console.error(
        `check-invariants --mutations: FAIL (${missing.length} missing, ${undetected.length} undetected)`,
      );
      return 1;
    }
    console.log(
      `check-invariants --mutations: PASS (${proven} planted violation(s) detected by their gates; ` +
        `${counts.releaseCritical} release-critical invariant(s) all proven; ` +
        `${counts.nonCriticalDebts} non-critical documented debt(s))`,
    );
  }
  console.log(
    `check-invariants: PASS (${counts.invariants} invariants, ${counts.witnesses} mutation witnesses, ` +
      `${counts.releaseCritical} release-critical (0 debt), ${counts.downgraded} downgraded, ` +
      `${counts.nonCriticalDebts} non-critical documented debt(s))`,
  );
  return 0;
}

const entryArgs = process.argv.slice(2);
if (entryArgs[0] === '--tree-barrier') {
  // Internal mode: continuous checkout hasher for the mutation campaign. The
  // parent terminates this process and reads its violation count.
  treeBarrier(entryArgs);
} else {
  process.exit(main());
}
