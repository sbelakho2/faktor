#!/usr/bin/env node
// Generic planted-mutation runner for tests/invariants.toml.
//
// A spec under scripts/mutations/ names the byte-level edits of production
// source and the gate command that must FAIL while those edits are live.
// Three shapes, all validated by the same rules below:
//
//   { id, file, find, replace, gate[, note, timeout_ms, expect] }
//     one anchored edit of one existing file (legacy shape);
//   { id, files: [{ file, find, replace }], gate[, …] }
//     several anchored edits, each `find` occurring EXACTLY ONCE;
//   { id, create: [{ file, content }], gate[, …] }
//     brand-new files that exist only for the gate run. `create` proves a
//     scanner is closed over NEW MODULES: the gate must discover the planted
//     file through the module tree although the proof manifest has never
//     seen it. Created files are removed (and edited files restored) in the
//     restore path before the runner decides.
//
// The runner:
//
//   1. snapshots every target file's bytes (or records that a target file
//      does not exist, for `create`);
//   2. refuses to run unless each `find` occurs EXACTLY ONCE in its file and
//      every `create` target is absent;
//   3. runs `gate` on the PRISTINE source and requires it to exit 0 (a gate
//      that is already failing proves nothing about the oracle);
//   4. applies `find` -> `replace` and writes every `create` file;
//   5. runs `gate` (expected non-zero with the mutation live, and matching
//      the spec's optional `expect` signature when one is declared);
//   6. restores the snapshots and removes created files in `finally` BEFORE
//      deciding the exit code (`process.exit` is never called inside the
//      try);
//   7. exits 0 ONLY when the pristine control passed AND the mutated gate
//      failed for the planted reason. Environment failures (missing
//      toolchain, exit 126/127, harness timeout), a mutation that does not
//      compile, and a failure that does not match `expect` are NEVER
//      counted as detection.
//
// Crash safety: SIGINT/SIGTERM/SIGHUP restore the snapshot and kill the active
// gate process group before exiting, so a mutated source is never left behind.
//
// Isolation (audit P0-CERT): mutation gates NEVER run against the trusted
// source tree. The runner copies the checkout into a scratch root (or uses
// FAKTOR_MUTATION_ROOT, prepared once by the campaign runner) and applies
// every planted edit THERE; the main tree's bytes are never touched, so a
// parallel lane can hash/compile it safely. CARGO_TARGET_DIR defaults to a
// dedicated cache outside the copy so compilations are reused across specs
// without ever writing into the trusted artifact tree.
//
// Modes:
//   node scripts/mutation-run.mjs <id>        run one spec (registry command)
//   node scripts/mutation-run.mjs --check-specs
//                                             validate every spec WITHOUT
//                                             mutating (safe for CI)
//   node scripts/mutation-run.mjs --list      list spec ids
//
// Exit codes: 0 mutation detected (or --check-specs/--list clean); 1 not
// proven (control not green, environment/toolchain failure, mutation did not
// compile, missing expected signature, or the gate passed with the mutation
// live); 2 usage or spec errors.

import { spawn } from 'node:child_process';
import { provisionSupport } from './mutation-support.mjs';
import { cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, isAbsolute, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
/** Scratch root the planted edits and gate commands run in. */
let GATE_ROOT = process.env.FAKTOR_MUTATION_ROOT
  ? resolve(process.env.FAKTOR_MUTATION_ROOT)
  : SCRIPT_ROOT;
let GATE_ROOT_CLEANUP = null;
const ISOLATION_EXCLUDES = new Set(['target', '.git', 'node_modules', '.idea']);
const SPEC_DIR = join(SCRIPT_ROOT, 'scripts/mutations');
const DEFAULT_TIMEOUT_MS = 30 * 60 * 1000;
const OUTPUT_TAIL_BYTES = 64 * 1024;

/** TRUE for any path with an excluded component below `root`. */
function excludedUnder(root, path) {
  const rel = relative(root, path);
  if (rel === '') return false;
  return rel.split(/[\\/]/).some((part) => ISOLATION_EXCLUDES.has(part));
}

/** Snapshot the gate support trees (see scripts/mutation-support.mjs). */
function provisionGateSupports(sourceRoot, scratch) {
  const record = provisionSupport(sourceRoot, scratch);
  if (record.cli !== null) {
    console.log(`mutation-support: cli=${record.cli.method} sha256=${record.cli.sha256}`);
  }
  if (record.node_modules !== null && record.node_modules.digest) {
    console.log(
      `mutation-support: node_modules=${record.node_modules.method} digest=${record.node_modules.digest} entries=${record.node_modules.entries}`,
    );
  }
}

/**
 * Prepare the scratch checkout when the campaign runner did not supply one:
 * a recursive copy of the CURRENT working tree (untracked files included, so
 * local verification sees exactly what the developer sees) minus build/VCS/
 * dependency trees. The copy is removed in `finally`.
 */
function prepareGateRoot() {
  if (process.env.FAKTOR_MUTATION_ROOT) {
    return;
  }
  const scratch = mkdtempSync(join(tmpdir(), 'faktor-mutation-'));
  cpSync(SCRIPT_ROOT, scratch, {
    recursive: true,
    filter: (source) => !excludedUnder(SCRIPT_ROOT, source),
  });
  provisionGateSupports(SCRIPT_ROOT, scratch);
  GATE_ROOT = scratch;
  GATE_ROOT_CLEANUP = () => rmSync(scratch, { recursive: true, force: true });
}

function cleanupGateRoot() {
  if (GATE_ROOT_CLEANUP !== null) {
    GATE_ROOT_CLEANUP();
    GATE_ROOT_CLEANUP = null;
  }
}

let active = null; // Array<{ mode: 'edit', abs, original } | { mode: 'create', abs }>
let activeChild = null;

function restore() {
  if (active !== null) {
    for (const entry of active) {
      if (entry.mode === 'create') rmSync(entry.abs, { force: true });
      else writeFileSync(entry.abs, entry.original);
    }
    active = null;
  }
}

function killActiveGate() {
  if (activeChild !== null && activeChild.pid !== undefined) {
    try {
      if (process.platform === 'win32') activeChild.kill('SIGKILL');
      else process.kill(-activeChild.pid, 'SIGKILL');
    } catch {
      try {
        activeChild.kill('SIGKILL');
      } catch {
        // already gone
      }
    }
  }
}

for (const [signal, code] of [
  ['SIGINT', 130],
  ['SIGTERM', 143],
  ['SIGHUP', 129],
]) {
  process.on(signal, () => {
    restore();
    killActiveGate();
    cleanupGateRoot();
    process.stderr.write(`mutation-run: ${signal} — snapshot restored, gate killed\n`);
    process.exit(code);
  });
}

function usage() {
  console.error(
    'usage: node scripts/mutation-run.mjs <id> | --check-specs | --list\n' +
      '  <id>            invariant/spec id, e.g. INV-COMPACTION-SUMMARY-CAP\n' +
      '  --check-specs   validate all specs without mutating any file\n' +
      '  --list          print all spec ids',
  );
}

// ------------------------------------------------------------ spec parsing

function unescapeQuoted(raw, source) {
  let out = '';
  let i = 1;
  while (i < raw.length) {
    const ch = raw[i];
    if (ch !== '\\') {
      out += ch;
      i += 1;
      continue;
    }
    const next = raw[i + 1];
    if (next === 'n') out += '\n';
    else if (next === 't') out += '\t';
    else if (next === 'r') out += '\r';
    else if (next === '"') out += '"';
    else if (next === '\\') out += '\\';
    else if (next === 'u') {
      const hex = raw.slice(i + 2, i + 6);
      if (!/^[0-9a-fA-F]{4}$/.test(hex)) {
        throw new Error(`bad \\u escape in ${source}`);
      }
      out += String.fromCharCode(parseInt(hex, 16));
      i += 4;
    } else {
      throw new Error(`unsupported escape '\\${next}' in ${source}`);
    }
    i += 2;
  }
  return out;
}

/** Minimal flat TOML reader: `key = "value"` lines plus comments/blank lines. */
function parseTomlSpec(text, source) {
  const spec = {};
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i].trim();
    if (line === '' || line.startsWith('#')) continue;
    const match = /^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+)$/.exec(line);
    if (!match) throw new Error(`${source}:${i + 1}: malformed line '${line}'`);
    const [, key, rest] = match;
    if (!rest.startsWith('"')) {
      throw new Error(`${source}:${i + 1}: '${key}' must be a quoted string`);
    }
    if (Object.prototype.hasOwnProperty.call(spec, key)) {
      throw new Error(`${source}:${i + 1}: duplicate key '${key}'`);
    }
    spec[key] = unescapeQuoted(rest.trim(), `${source}:${i + 1}`);
  }
  return spec;
}

function loadSpecs() {
  const specs = [];
  const problems = [];
  if (!existsSync(SPEC_DIR)) {
    return { specs, problems: [`spec directory ${SPEC_DIR} does not exist`] };
  }
  for (const name of readdirSync(SPEC_DIR).sort()) {
    const isJson = name.endsWith('.json');
    const isToml = name.endsWith('.toml');
    if (!isJson && !isToml) continue;
    const path = join(SPEC_DIR, name);
    try {
      const text = readFileSync(path, 'utf8');
      const spec = isJson ? JSON.parse(text) : parseTomlSpec(text, name);
      if (spec === null || typeof spec !== 'object' || Array.isArray(spec)) {
        throw new Error('spec must be an object');
      }
      spec.__file = name;
      specs.push(spec);
    } catch (error) {
      problems.push(`${name}: ${error.message}`);
    }
  }
  return { specs, problems };
}

/**
 * The spec's edit shape, decided by property PRESENCE so a malformed array is
 * reported instead of silently falling through to the legacy fields:
 * `create` (brand-new files), `files` (several anchored edits) or the legacy
 * single `{file, find, replace}` edit.
 */
function specMode(spec) {
  if (Object.prototype.hasOwnProperty.call(spec, 'create')) return 'create';
  if (Object.prototype.hasOwnProperty.call(spec, 'files')) return 'files';
  return 'legacy';
}

/** Normalized `[{ mode, file, find?, replace?, content? }]` for a valid spec. */
function specEdits(spec) {
  const mode = specMode(spec);
  if (mode === 'create') {
    return (Array.isArray(spec.create) ? spec.create : []).map((entry) => ({
      mode: 'create',
      file: entry?.file,
      content: entry?.content,
    }));
  }
  if (mode === 'files') {
    return (Array.isArray(spec.files) ? spec.files : []).map((entry) => ({
      mode: 'edit',
      file: entry?.file,
      find: entry?.find,
      replace: entry?.replace,
    }));
  }
  return [{ mode: 'edit', file: spec.file, find: spec.find, replace: spec.replace }];
}

/** `INV-…`-shaped human summary of a spec's targets, for --list. */
function specTargets(spec) {
  if (specMode(spec) === 'legacy') return spec.file;
  const entries = Array.isArray(spec[specMode(spec)]) ? spec[specMode(spec)] : [];
  return `${specMode(spec)}(${entries.map((entry) => entry?.file).join(', ')})`;
}

/** TRUE when `file` resolves inside the repository root. */
function staysInsideRoot(file) {
  const rel = relative(SCRIPT_ROOT, resolve(SCRIPT_ROOT, file));
  return rel !== '' && !rel.startsWith('..') && !isAbsolute(rel);
}

/**
 * Validate one spec. With `checkFind` every anchored target file must exist
 * and its `find` must occur exactly once, and every `create` target must NOT
 * exist yet (the --check-specs contract, no mutation).
 */
function validateSpec(spec, checkFind) {
  const problems = [];
  const where = spec.__file ?? spec.id ?? '<unknown>';
  const mode = specMode(spec);
  for (const field of ['id', 'gate']) {
    if (typeof spec[field] !== 'string' || spec[field].trim() === '') {
      problems.push(`${where}: missing non-empty '${field}'`);
    }
  }
  if (problems.length > 0) return problems;
  if (!/^INV-[A-Z0-9-]+$/.test(spec.id)) {
    problems.push(`${where}: id '${spec.id}' is not an INV-* id`);
  }
  if (Object.prototype.hasOwnProperty.call(spec, 'create') && Object.prototype.hasOwnProperty.call(spec, 'files')) {
    problems.push(`${where}: 'create' and 'files' are mutually exclusive`);
  }
  if (mode === 'legacy') {
    for (const field of ['file', 'find', 'replace']) {
      if (typeof spec[field] !== 'string' || spec[field].trim() === '') {
        problems.push(`${where}: missing non-empty '${field}'`);
      }
    }
    if (typeof spec.file === 'string' && spec.file.trim() !== '' && !staysInsideRoot(spec.file)) {
      problems.push(`${where}: file ${JSON.stringify(spec.file)} escapes the repository`);
    }
    if (
      typeof spec.find === 'string' &&
      typeof spec.replace === 'string' &&
      spec.find === spec.replace
    ) {
      problems.push(`${where}: find and replace are identical; the spec mutates nothing`);
    }
  } else {
    for (const field of ['file', 'find', 'replace']) {
      if (Object.prototype.hasOwnProperty.call(spec, field)) {
        problems.push(`${where}: '${field}' cannot be combined with '${mode}'`);
      }
    }
    const entries = Array.isArray(spec[mode]) ? spec[mode] : null;
    if (entries === null || entries.length === 0) {
      problems.push(`${where}: '${mode}' must be a non-empty array of edits`);
    } else {
      for (const [index, entry] of entries.entries()) {
        const at = `${where}: ${mode}[${index}]`;
        if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
          problems.push(`${at} must be an object`);
          continue;
        }
        if (typeof entry.file !== 'string' || entry.file.trim() === '') {
          problems.push(`${at} needs a non-empty 'file'`);
        } else if (!staysInsideRoot(entry.file)) {
          problems.push(`${at} file ${JSON.stringify(entry.file)} escapes the repository`);
        }
        if (mode === 'create') {
          if (typeof entry.content !== 'string') {
            problems.push(`${at} needs a string 'content'`);
          }
        } else {
          for (const field of ['find', 'replace']) {
            if (typeof entry[field] !== 'string') {
              problems.push(`${at} needs a string '${field}'`);
            }
          }
          if (
            typeof entry.find === 'string' &&
            typeof entry.replace === 'string' &&
            entry.find === entry.replace
          ) {
            problems.push(`${at}: find and replace are identical; the spec mutates nothing`);
          }
        }
      }
    }
  }
  if (spec.gate.includes('\n')) {
    problems.push(`${where}: gate must be a single command line`);
  }
  if (spec.expect !== undefined) {
    if (typeof spec.expect !== 'string' || spec.expect.trim() === '') {
      problems.push(`${where}: 'expect' must be a non-empty regex string when present`);
    } else {
      try {
        new RegExp(spec.expect);
      } catch (error) {
        problems.push(`${where}: 'expect' is not a valid regex: ${error.message}`);
      }
    }
  }
  if (checkFind && problems.length === 0) {
    for (const edit of specEdits(spec)) {
      const abs = resolve(SCRIPT_ROOT, edit.file);
      if (edit.mode === 'create') {
        if (existsSync(abs)) {
          problems.push(
            `${where}: create target ${edit.file} already exists (a planted NEW module must not pre-exist)`,
          );
        }
        continue;
      }
      if (!existsSync(abs)) {
        problems.push(`${where}: target file ${edit.file} does not exist`);
      } else {
        const haystack = readFileSync(abs);
        const needle = Buffer.from(edit.find, 'utf8');
        const first = haystack.indexOf(needle);
        if (first === -1) {
          problems.push(`${where}: find anchor not found in ${edit.file}`);
        } else if (haystack.indexOf(needle, first + 1) !== -1) {
          problems.push(`${where}: find anchor is ambiguous in ${edit.file} (occurs more than once)`);
        }
      }
    }
  }
  return problems;
}

function checkSpecs() {
  const { specs, problems: loadProblems } = loadSpecs();
  const problems = [...loadProblems];
  const ids = new Map();
  for (const spec of specs) {
    problems.push(...validateSpec(spec, true));
    if (typeof spec.id === 'string') {
      if (ids.has(spec.id)) {
        problems.push(`${spec.__file}: duplicate spec id '${spec.id}' (also in ${ids.get(spec.id)})`);
      } else {
        ids.set(spec.id, spec.__file);
      }
    }
  }
  if (problems.length > 0) {
    for (const problem of problems) console.error(`mutation-spec: ${problem}`);
    console.error(`check-specs: FAIL (${problems.length} problem(s) over ${specs.length} spec(s))`);
    return 2;
  }
  console.log(`check-specs: PASS (${specs.length} spec(s), every find anchor unique, no file mutated)`);
  return 0;
}

// --------------------------------------------------------------- gate run

function runGate(spec) {
  return new Promise((settle) => {
    const timeoutMs =
      Number.isInteger(spec.timeout_ms) && spec.timeout_ms > 0 ? spec.timeout_ms : DEFAULT_TIMEOUT_MS;
    const env = { ...process.env };
    if (!env.CARGO_TARGET_DIR) {
      // Never compile mutants into the trusted target/ tree: a dedicated
      // cache outside the scratch copy, reused across specs.
      env.CARGO_TARGET_DIR = join(SCRIPT_ROOT, 'target', 'mutation-campaign');
    }
    const child = spawn(spec.gate, {
      shell: true,
      cwd: GATE_ROOT,
      env,
      detached: process.platform !== 'win32',
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    activeChild = child;
    let tail = '';
    const collect = (chunk) => {
      tail += chunk.toString('utf8');
      if (tail.length > OUTPUT_TAIL_BYTES) tail = tail.slice(tail.length - OUTPUT_TAIL_BYTES);
    };
    child.stdout.on('data', collect);
    child.stderr.on('data', collect);
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      try {
        if (process.platform === 'win32') child.kill('SIGKILL');
        else process.kill(-child.pid, 'SIGKILL');
      } catch {
        child.kill('SIGKILL');
      }
    }, timeoutMs);
    child.on('error', (error) => {
      clearTimeout(timer);
      activeChild = null;
      settle({ error: error.message, tail });
    });
    child.on('close', (code, signal) => {
      clearTimeout(timer);
      activeChild = null;
      settle({ code, signal, timedOut, tail });
    });
  });
}

// ------------------------------------------------------- result semantics

/** `exit N` / `signal N` for a finished gate run. */
function howOf(result) {
  return result.code === null ? `signal ${result.signal}` : `exit ${result.code}`;
}

/**
 * A toolchain/environment failure, never a detection: the shell's command
 * not-found codes and their stderr spellings.
 */
function looksLikeToolchainMissing(result) {
  if (result.code === 126 || result.code === 127) return true;
  return /(command not found|not recognized as an internal or external command|executable file not found|env: .*: No such file or directory)/i.test(
    result.tail ?? '',
  );
}

/**
 * The mutated source did not compile, so the oracle never executed. Cargo,
 * rustc and Gradle spellings.
 */
function looksLikeCompileFailure(output) {
  return /(could not compile|error\[E[0-9]+\]|Compilation failure|Compilation error|cannot find crate)/i.test(
    output,
  );
}

/** Non-null reason when a pristine control run is not a green baseline. */
function controlProblem(result) {
  if (result.error !== undefined) {
    return `the control gate could not be spawned: ${result.error}`;
  }
  if (result.timedOut) {
    return 'the control gate hit the harness timeout on the pristine source';
  }
  if (looksLikeToolchainMissing(result)) {
    return `the gate toolchain is unavailable on the pristine source (${howOf(result)})`;
  }
  if (result.code !== 0) {
    return `the control gate failed on the pristine source (${howOf(result)}); a gate that is not green before the mutation proves nothing`;
  }
  return null;
}

// ------------------------------------------------------------------- main

async function runOne(id) {
  const { specs, problems: loadProblems } = loadSpecs();
  if (loadProblems.length > 0) {
    for (const problem of loadProblems) console.error(`mutation-spec: ${problem}`);
    return 2;
  }
  const matches = specs.filter((spec) => spec.id === id);
  if (matches.length === 0) {
    console.error(`mutation-run: no spec with id '${id}' under scripts/mutations/`);
    return 2;
  }
  if (matches.length > 1) {
    console.error(`mutation-run: ${matches.length} specs share id '${id}'`);
    return 2;
  }
  const spec = matches[0];
  const problems = validateSpec(spec, true);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`mutation-run: ${problem}`);
    return 2;
  }

  // The planted edits and both gate runs happen in the ISOLATED scratch root:
  // the trusted tree is never mutated, even transiently.
  prepareGateRoot();
  try {
    const planned = [];
    for (const edit of specEdits(spec)) {
      const abs = resolve(GATE_ROOT, edit.file);
      if (edit.mode === 'create') {
        if (existsSync(abs)) {
          console.error(`mutation-run: create target ${edit.file} already exists in the scratch copy`);
          return 2;
        }
        planned.push({ mode: 'create', abs, content: edit.content });
        continue;
      }
      const original = readFileSync(abs);
      const find = Buffer.from(edit.find, 'utf8');
      if (original.indexOf(find) === -1) {
        console.error(`mutation-run: find anchor not found in the scratch copy of ${edit.file}`);
        return 2;
      }
      const at = original.indexOf(find);
      planned.push({
        mode: 'edit',
        abs,
        original,
        mutated: Buffer.concat([
          original.subarray(0, at),
          Buffer.from(edit.replace, 'utf8'),
          original.subarray(at + find.length),
        ]),
      });
    }

    // Pristine control run: the gate must be green BEFORE the mutation, or a
    // failure with the mutation live says nothing about the oracle.
    const control = await runGate(spec);
    const controlReason = controlProblem(control);
    if (controlReason !== null) {
      console.error(`mutation-run: ${spec.id} FAILED — ${controlReason}`);
      if ((control.tail ?? '').trim() !== '') {
        console.error(`mutation-run: control output tail:\n${control.tail.trimEnd()}`);
      }
      return 1;
    }

    let detected = false;
    let reason = '';
    let tail = '';
    active = planned;
    try {
      for (const entry of planned) {
        if (entry.mode === 'create') {
          mkdirSync(dirname(entry.abs), { recursive: true });
          writeFileSync(entry.abs, entry.content);
        } else {
          writeFileSync(entry.abs, entry.mutated);
        }
      }
      const result = await runGate(spec);
      tail = result.tail ?? '';
      const how = howOf(result);
      if (result.error !== undefined) {
        reason = `the gate could not be spawned: ${result.error}`;
      } else if (result.timedOut) {
        reason = 'the gate hit the harness timeout while the mutation was live';
      } else if (looksLikeToolchainMissing(result)) {
        reason = `the gate toolchain is unavailable (${how}); an environment failure is not a detection`;
      } else if (result.code === 0) {
        reason = `planted mutation NOT detected: gate '${spec.gate}' exited 0 with the violation live`;
      } else if (looksLikeCompileFailure(result.tail ?? '')) {
        reason = `the mutated source does not compile (${how}); the oracle never ran`;
      } else if (spec.expect !== undefined && !new RegExp(spec.expect).test(result.tail ?? '')) {
        reason = `the gate failed (${how}) without the expected signature /${spec.expect}/; a failure for another reason is not a detection`;
      } else {
        detected = true;
        console.log(`mutation-run: ${spec.id} detected by gate (${how})`);
      }
    } finally {
      restore();
    }

    if (detected) return 0;
    console.error(`mutation-run: ${spec.id} FAILED — ${reason}`);
    if (tail.trim() !== '') {
      console.error(`mutation-run: gate output tail:\n${tail.trimEnd()}`);
    }
    return 1;
  } finally {
    cleanupGateRoot();
  }
}

async function main() {
  const args = process.argv.slice(2);
  if (args.length === 0 || args.includes('-h') || args.includes('--help')) {
    usage();
    return 2;
  }
  if (args[0] === '--check-specs') return checkSpecs();
  if (args[0] === '--list') {
    const { specs, problems } = loadSpecs();
    for (const problem of problems) console.error(`mutation-spec: ${problem}`);
    if (problems.length > 0) return 2;
    for (const spec of specs) console.log(`${spec.id}  ${specTargets(spec)}`);
    return 0;
  }
  return runOne(args[0]);
}

main().then(
  (code) => {
    process.exitCode = code;
  },
  (error) => {
    restore();
    cleanupGateRoot();
    console.error(`mutation-run: fatal: ${error && error.stack ? error.stack : error}`);
    process.exitCode = 1;
  },
);
