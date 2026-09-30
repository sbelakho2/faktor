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
//   3. `script_check` entries (non-Rust gates) must resolve to a literal
//      VS Code selftest step label (`apps/vscode/scripts/selftest.mjs`) or to
//      a command line wired in a `.woodpecker` workflow;
//   4. ids are unique `INV-...` and `platforms` is non-empty.
//
// Usage:
//   node scripts/check-invariants.mjs [--root DIR] [--registry FILE]
//   node scripts/check-invariants.mjs --root FIXTURE --registry FIXTURE.toml --expect-fail CODE
//
// Exit codes: 0 pass (or expected failure matched); 1 violations; 2 usage.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
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
  const add = (code, detail) => problems.push(`${code}: ${detail}`);

  if (!existsSync(registryPath)) {
    add('missing-registry', `${relative(SCRIPT_ROOT, registryPath)} does not exist`);
    return { problems, counts: { invariants: 0, witnesses: 0, debts: 0 } };
  }
  const { entries, problems: parseProblems } = parseRegistry(readFileSync(registryPath, 'utf8'));
  for (const problem of parseProblems) {
    add('parse-error', `line ${problem.line}: ${problem.message}`);
  }
  if (parseProblems.length > 0) {
    return { problems, counts: { invariants: entries.length, witnesses: 0, debts: 0 } };
  }

  const fnIndex = buildFnIndex(root);
  const labels = selftestLabels(root);
  const commands = ciCommands(root);
  const hasScriptCheck = (value) =>
    labels.includes(value) || commands.some((command) => command.includes(value));

  const ids = new Set();
  let witnesses = 0;
  let debts = 0;

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
          `${id} script_check '${scriptCheck}' matches no VS Code selftest step and no .woodpecker command line`,
        );
      }
      continue;
    }

    for (const field of ['unit_test', 'production_wiring']) {
      const value = typeof entry[field] === 'string' ? entry[field].trim() : '';
      if (value === '') {
        add('missing-field', `${id} requires '${field}' (or a script_check)`);
      } else {
        requireFn(entry, field, value);
      }
    }

    const witness = typeof entry.mutation_witness === 'string' ? entry.mutation_witness.trim() : '';
    const debt = typeof entry.mutation_debt === 'string' ? entry.mutation_debt.trim() : '';
    if (witness === '' && debt === '') {
      add('missing-witness-or-debt', `${id} needs a mutation_witness fn or a mutation_debt reason`);
    }
    if (witness !== '' && debt !== '') {
      add('witness-and-debt', `${id} sets both mutation_witness '${witness}' and mutation_debt`);
    }
    if (witness !== '') {
      witnesses += 1;
      requireFn(entry, 'mutation_witness', witness);
    } else if (debt !== '') {
      debts += 1;
    }

    if (typeof entry.fault === 'string' && entry.fault.trim() !== '') {
      requireFn(entry, 'fault', entry.fault.trim());
    }
  }

  return { problems, counts: { invariants: entries.length, witnesses, debts } };
}

function main() {
  const args = process.argv.slice(2);
  if (args.includes('-h') || args.includes('--help')) {
    console.log(
      'usage: node scripts/check-invariants.mjs [--root DIR] [--registry FILE] [--expect-fail CODE]',
    );
    return 2;
  }
  const root = resolve(argValue(args, '--root', SCRIPT_ROOT));
  const registryPath = resolve(argValue(args, '--registry', join(root, 'tests/invariants.toml')));
  const { problems, counts } = runCheck({ root, registryPath });
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
  console.log(
    `check-invariants: PASS (${counts.invariants} invariants, ${counts.witnesses} mutation witnesses, ${counts.debts} documented debts)`,
  );
  return 0;
}

process.exit(main());
