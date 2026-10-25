#!/usr/bin/env node
// Structural testing-separation gate.
//
// The SUT (daemon/agent/UI) must never score, attest, or fabricate its own
// test expectations. This checker scans every test tree in the repository
// (Rust test modules and integration tests, Node/Kotlin host harnesses,
// certification scripts and mutation specs) for the five violation classes
// in docs/testing-separation.md and requires new test files to declare their
// oracle type and isolation.
//
// Usage:
//   node scripts/check-test-separation.mjs --check
//   node scripts/check-test-separation.mjs --selftest
//   node scripts/check-test-separation.mjs --write-baseline   (reviewed)
//
// Exit codes: 0 clean; 1 violations; 2 usage/baseline/selftest failure.

import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT = fileURLToPath(import.meta.url);
const ROOT = resolve(dirname(SCRIPT), '..');
const BASELINE_PATH = join(ROOT, 'tests', 'test-separation', 'baseline.json');
const FIXTURE_ROOT = join(ROOT, 'tests', 'test-separation', 'fixtures');
const SELF_REL = relative(ROOT, SCRIPT).split(sep).join('/');
const BASELINE_REL = relative(ROOT, BASELINE_PATH).split(sep).join('/');
const FIXTURE_REL = relative(ROOT, FIXTURE_ROOT).split(sep).join('/');
const SCHEMA = 'faktor-test-separation/v1';

const ORACLE_TYPES = new Set([
  'external-fixture',
  'independent-reference',
  'property',
  'contract',
  'mutation',
  'differential',
]);
const ISOLATIONS = new Set([
  'separate-process',
  'isolated-data-dir',
  'isolated-worktree',
  'isolated-browser-profile',
  'read-only-artifacts',
  'pure-unit',
]);
const DECLARATION = /TEST-SEPARATION:\s*oracle=([a-z0-9-]+)\s+isolation=([a-z0-9-]+(?:\s*,\s*[a-z0-9-]+)*)/;

const CODE_EXTENSIONS = new Set(['.rs', '.mjs', '.js', '.cjs', '.ts', '.tsx', '.kt', '.kts']);
const SKIP_DIRS = new Set(['.git', 'target', 'node_modules', 'build', 'out', '.gradle', '.idea', 'dist']);

const METRIC_SETTER =
  /\.with_(?:score|scored|pass|passed|passing|verified|success|succeeded|fail|failed|failure|attest|attested|attestation|oracle|physics|authority|missing|missing_info|boundary|boundaries|uncertainty|trap|experiment|refusal|metric|metrics|verdict)\w*\s*\(/gi;
const SELF_VERDICT = /\.(?:is_verified|is_verified_with_verifier|is_self_verified|is_attested|is_self_attested|self_attested|verify_with_verifier)\s*\(/g;
const DATA_ENV = /(?:FAKTOR_(?:DATA_DIR|HOME|CONFIG_DIR|STATE_DIR|CACHE_DIR)|FAKTOR_HOME)/;
const SUT_VALUE =
  /\b(?:observed|sut|sut_result|sut_answer|result|outcome|response|report|receipt|case\.sut|fixture\.sut|\.sut\b)\b/i;
const INDEPENDENT_RHS =
  /\b(?:oracle|expectation|expectations|expected_fixture|golden|fixture_expected|reference)\b/i;

function toRel(abs) {
  return relative(ROOT, abs).split(sep).join('/');
}

// ------------------------------------------------------------------ masking

/**
 * Mask comments and string/template/char literals while preserving offsets
 * and newlines. Code inside a masked literal (a fixture, an assertion message,
 * a documentation string) can therefore never fire a scan.
 */
function maskSource(src, ext) {
  const out = src.split('');
  const n = src.length;
  const blank = (from, to) => {
    for (let i = from; i < to && i < n; i += 1) {
      if (out[i] !== '\n') out[i] = ' ';
    }
  };
  let i = 0;
  const rust = ext === '.rs';
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      let j = i;
      while (j < n && src[j] !== '\n') j += 1;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '/' && d === '*') {
      let j = i + 2;
      while (j < n && !(src[j] === '*' && src[j + 1] === '/')) j += 1;
      j = Math.min(n, j + 2);
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '"' && rust) {
      // Raw strings r"…", r#"…"# / br#"…"#, and ordinary strings.
      let start = i;
      let hashes = 0;
      if (c === 'r') {
        let k = i + 1;
        while (src[k] === '#') {
          hashes += 1;
          k += 1;
        }
        if (src[k] === '"') {
          start = i;
          i = k + 1;
          const endNeedle = `"${'#'.repeat(hashes)}`;
          const end = src.indexOf(endNeedle, i);
          i = end === -1 ? n : end + endNeedle.length;
          blank(start, i);
          continue;
        }
      }
      if (c === '"') {
        let j = i + 1;
        while (j < n) {
          if (src[j] === '\\') {
            j += 2;
            continue;
          }
          if (src[j] === '"') {
            j += 1;
            break;
          }
          j += 1;
        }
        blank(i, j);
        i = j;
        continue;
      }
    }
    if ((c === 'b' || c === 'r') && rust) {
      // b"…", br#"…"#
      thisPrefix: {
        let k = i;
        if (src[k] === 'b') k += 1;
        if (src[k] === 'r') {
          k += 1;
          let hashes = 0;
          while (src[k] === '#') {
            hashes += 1;
            k += 1;
          }
          if (src[k] === '"') {
            const start = i;
            i = k + 1;
            const endNeedle = `"${'#'.repeat(hashes)}`;
            const end = src.indexOf(endNeedle, i);
            i = end === -1 ? n : end + endNeedle.length;
            blank(start, i);
            continue;
          }
          break thisPrefix;
        }
        if (src[k] === '"') {
          const start = i;
          i = k + 1;
          while (i < n) {
            if (src[i] === '\\') {
              i += 2;
              continue;
            }
            if (src[i] === '"') {
              i += 1;
              break;
            }
            i += 1;
          }
          blank(start, i);
          continue;
        }
      }
    }
    if (c === '"' || c === "'" || c === '`') {
      // JS/TS strings/templates and Kotlin char/string literals. A template
      // literal is masked whole; code-like fixture text inside it is data.
      const quote = c;
      let j = i + 1;
      while (j < n) {
        if (src[j] === '\\') {
          j += 2;
          continue;
        }
        if (src[j] === quote) {
          j += 1;
          break;
        }
        j += 1;
      }
      blank(i, j);
      i = j;
      continue;
    }
    i += 1;
  }
  return out.join('');
}

/** Mask only comments, keeping string/argument identity for oracle checks. */
function maskComments(src, ext) {
  const out = src.split('');
  const n = src.length;
  const blank = (from, to) => {
    for (let i = from; i < to && i < n; i += 1) if (out[i] !== '\n') out[i] = ' ';
  };
  let i = 0;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      let j = i;
      while (j < n && src[j] !== '\n') j += 1;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '/' && d === '*') {
      let j = i + 2;
      while (j < n && !(src[j] === '*' && src[j + 1] === '/')) j += 1;
      j = Math.min(n, j + 2);
      blank(i, j);
      i = j;
      continue;
    }
    i += 1;
  }
  return out.join('');
}

function isDeterminismContext(source, index) {
  const before = source.slice(Math.max(0, index - 4000), index);
  const names = [...before.matchAll(/\bfn\s+([A-Za-z_][A-Za-z0-9_]*)/g)];
  const name = names.length === 0 ? '' : names[names.length - 1][1];
  if (!/determin|stable|idempot|reproduc|byte_identical|same|twice|fold|normali[sz]|collision|distinct|differ|invariant/i.test(name)) {
    return false;
  }
  // A determinism-named test may assert f(x) == f(x); the expectation is the
  // property itself, not a value manufactured from the SUT result.
  return true;
}

function lineAt(src, index) {
  let line = 1;
  for (let i = 0; i < index && i < src.length; i += 1) {
    if (src[i] === '\n') line += 1;
  }
  return line;
}

// --------------------------------------------------------------- discovery

function walk(dir, out) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return;
  }
  for (const name of entries) {
    if (SKIP_DIRS.has(name)) continue;
    const abs = join(dir, name);
    let info;
    try {
      info = statSync(abs);
    } catch {
      continue;
    }
    if (info.isDirectory()) {
      walk(abs, out);
    } else if (info.isFile()) {
      out.push(abs);
    }
  }
}

function under(rel, prefix) {
  return rel === prefix || rel.startsWith(`${prefix}/`);
}

function hasTestAttribute(text) {
  return /#\[cfg\([^\]]*\btest\b[^\]]*\)\]/.test(text) || /#\[(?:tokio::)?test\b/.test(text);
}

/**
 * Is this path part of a test suite? The classification is deliberately
 * broader than Cargo's test layout: UI host scripts, verification scripts and
 * mutation specs are test code too.
 */
function isCandidate(rel, text, ext) {
  if (rel === SELF_REL || rel === BASELINE_REL || under(rel, FIXTURE_REL)) return false;
  if (rel.startsWith('docs/') || rel.startsWith('ui/LICENSES/')) return false;
  if (!CODE_EXTENSIONS.has(ext)) return false;
  if (under(rel, 'tests')) return true;
  if (/^[a-z0-9-]+\/tests\//.test(rel)) return true;
  if (rel.startsWith('crates/')) {
    if (/\/tests\//.test(rel)) return true;
    if (/\/src\/.*(?:^|\/)(?:tests?|test_[^/]*|[^/]*_tests?|test_support|testsupport|testing|fixtures)\.rs$/.test(rel)) return true;
    if (ext === '.rs' && hasTestAttribute(text)) return true;
  }
  if (rel.startsWith('apps/')) {
    if (/\/scripts\//.test(rel)) return true;
    if (/\/src\/test\//.test(rel)) return true;
    if (/(?:Test|Tests|Smoke|smoke|test|tests)\.(?:kt|kts|ts|mjs|js)$/.test(rel)) return true;
  }
  if (rel.startsWith('scripts/')) {
    if (/^scripts\/mutation-/.test(rel)) return true;
    if (/^scripts\/(?:edition5|edition8)\//.test(rel)) return true;
    if (/^scripts\/(?:dogfood|vscode-e2e)/.test(rel)) return true;
    if (/^scripts\/certification\/tests\//.test(rel)) return true;
    if (/selftest/i.test(rel)) return true;
  }
  return false;
}

function discover(root = ROOT) {
  const all = [];
  walk(root, all);
  const files = [];
  for (const abs of all) {
    const rel = toRel(abs);
    const ext = rel.slice(rel.lastIndexOf('.'));
    let text;
    try {
      text = readFileSync(abs, 'utf8');
    } catch {
      continue;
    }
    if (isCandidate(rel, text, ext)) files.push({ rel, text, abs });
  }
  files.sort((a, b) => a.rel.localeCompare(b.rel));
  return files;
}

// ------------------------------------------------------------ declarations

function declarationOf(text) {
  const match = DECLARATION.exec(text);
  if (!match) return null;
  return {
    oracle: match[1],
    isolation: match[2].split(',').map((value) => value.trim()).filter(Boolean),
  };
}

/** Count test entry points; growth in a baseline file needs a declaration. */
function testCount(rel, text) {
  const ext = rel.slice(rel.lastIndexOf('.'));
  if (ext === '.rs') {
    return (text.match(/#\[(?:tokio::)?test\b/g) ?? []).length;
  }
  if (ext === '.kt' || ext === '.kts') {
    return (text.match(/@Test\b/g) ?? []).length;
  }
  return (text.match(/\b(?:test|it)\s*\(/g) ?? []).length;
}

function validDeclaration(decl) {
  const problems = [];
  if (!ORACLE_TYPES.has(decl.oracle)) {
    problems.push(`unknown oracle type '${decl.oracle}' (expected ${[...ORACLE_TYPES].join('|')})`);
  }
  for (const isolation of decl.isolation) {
    if (!ISOLATIONS.has(isolation)) {
      problems.push(`unknown isolation '${isolation}' (expected ${[...ISOLATIONS].join('|')})`);
    }
  }
  if (decl.isolation.length === 0) problems.push('declaration needs at least one isolation');
  if (decl.isolation.includes('pure-unit') && decl.isolation.length > 1) {
    problems.push('pure-unit cannot be combined with another isolation');
  }
  return problems;
}

function loadBaseline() {
  if (!existsSync(BASELINE_PATH)) {
    throw new Error(`missing baseline ${BASELINE_REL}; run --write-baseline after review`);
  }
  const parsed = JSON.parse(readFileSync(BASELINE_PATH, 'utf8'));
  if (parsed.schema !== SCHEMA) {
    throw new Error(`baseline schema '${parsed.schema}' is not ${SCHEMA}`);
  }
  if (parsed.files === null || typeof parsed.files !== 'object' || Array.isArray(parsed.files)) {
    throw new Error('baseline.files must be an object keyed by repository-relative path');
  }
  for (const [rel, entry] of Object.entries(parsed.files)) {
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
      throw new Error(`baseline entry ${rel} must be an object`);
    }
    const problems = validDeclaration(entry);
    if (problems.length > 0) throw new Error(`baseline entry ${rel}: ${problems.join('; ')}`);
  }
  return parsed;
}

/**
 * Resolve and validate one file's declaration. The inline declaration is
 * authoritative; an existing baseline entry supplies the inherited one. A
 * new file without either is rejected before any content scan.
 */
function resolveDeclaration(rel, text, baseline) {
  const inline = declarationOf(text);
  const entry = baseline.files[rel];
  if (inline === null && entry === undefined) {
    return {
      problems: [
        'new test file has no TEST-SEPARATION declaration; add `TEST-SEPARATION: oracle=<type> isolation=<type>`',
      ],
    };
  }
  const declaration = inline ?? entry;
  const problems = validDeclaration(declaration);
  if (inline !== null && entry !== undefined && entry.oracle !== inline.oracle) {
    problems.push(`inline oracle '${inline.oracle}' disagrees with baseline '${entry.oracle}'`);
  }
  if (inline === null && entry !== undefined && Number.isInteger(entry.tests)) {
    const current = testCount(rel, text);
    if (current > entry.tests) {
      problems.push(
        `file grew from ${entry.tests} to ${current} test(s) without an inline declaration; declare the new test's oracle/isolation`,
      );
    }
  }
  return { declaration, problems };
}

function defaultOracle(rel) {
  if (under(rel, 'tests/supply-engineering-traps')) {
    return { oracle: 'independent-reference', isolation: ['separate-process'] };
  }
  if (under(rel, 'scripts/mutations') || /^scripts\/mutation-/.test(rel)) {
    return { oracle: 'mutation', isolation: ['isolated-worktree', 'read-only-artifacts'] };
  }
  if (rel.startsWith('apps/')) {
    if (/\.(?:kt|kts)$/.test(rel)) {
      return { oracle: 'external-fixture', isolation: ['separate-process', 'read-only-artifacts'] };
    }
    return { oracle: 'external-fixture', isolation: ['isolated-browser-profile', 'read-only-artifacts'] };
  }
  if (rel.startsWith('crates/payments')) {
    return { oracle: 'external-fixture', isolation: ['isolated-data-dir', 'separate-process'] };
  }
  if (/compaction|handoff/.test(rel)) {
    return { oracle: 'external-fixture', isolation: ['isolated-data-dir'] };
  }
  if (rel.startsWith('tests/')) {
    return { oracle: 'independent-reference', isolation: ['isolated-data-dir'] };
  }
  if (rel.startsWith('crates/')) {
    return { oracle: 'external-fixture', isolation: ['isolated-data-dir'] };
  }
  return { oracle: 'independent-reference', isolation: ['pure-unit'] };
}

// ---------------------------------------------------------------- analysis

/**
 * Only test code is analyzed. For an inline Rust test module at the bottom of
 * a production file, the production half is masked out; integration tests and
 * test-named files are analyzed whole.
 */
function braceEnd(masked, open) {
  let depth = 0;
  for (let i = open; i < masked.length; i += 1) {
    if (masked[i] === '{') depth += 1;
    else if (masked[i] === '}') {
      depth -= 1;
      if (depth === 0) return i + 1;
    }
  }
  return masked.length;
}

function testCode(rel, text) {
  if (!rel.startsWith('crates/')) return text;
  if (/\/tests\//.test(rel)) return text;
  if (/(?:^|\/)(?:tests?|test_[^/]*|[^/]*_tests?|test_support|testsupport|testing|fixtures)\.rs$/.test(rel)) {
    return text;
  }
  const masked = maskSource(text, '.rs');
  const out = [...text.replace(/[^\n]/g, ' ')];
  const attributes = [
    ...text.matchAll(/#\[cfg\([^\]]*\btest\b[^\]]*\)\]/g),
    ...text.matchAll(/#\[(?:tokio::)?test\b[^\]]*\]/g),
  ];
  for (const match of attributes) {
    let at = match.index + match[0].length;
    while (at < masked.length && /\s/.test(masked[at])) at += 1;
    if (masked[at] === '[') continue;
    const open = masked.indexOf('{', at);
    if (open === -1) continue;
    const end = braceEnd(masked, open);
    for (let i = match.index; i < end; i += 1) out[i] = text[i];
  }
  return out.join('');
}
function normalizeCall(expr) {
  return expr
    .replace(/\s+/g, '')
    .replace(/^&+/, '')
    .replace(/\.clone\(\)$/, '');
}

function analyze(rel, text) {
  const ext = rel.slice(rel.lastIndexOf('.'));
  const scoped = testCode(rel, text);
  const masked = maskSource(scoped, ext);
  const commentless = maskComments(scoped, ext);
  const offenders = [];
  const add = (rule, index, message) => {
    const line = lineAt(text, index);
    const lines = text.split('\n');
    let exempt = false;
    for (let at = Math.max(0, line - 4); at < Math.min(lines.length, line); at += 1) {
      if (
        /TEST-SEPARATION-EXEMPT:/.test(lines[at]) &&
        lines[at].includes(rule) &&
        /[—-]\s*\S/.test(lines[at])
      ) {
        exempt = true;
        break;
      }
    }
    if (!exempt) offenders.push({ rule, file: rel, line, message });
  };

  // (a) a test-constructed expected value derived from the SUT observation.
  const assignment = /(?:let|const|var)\s+(?:mut\s+)?(expected[\w]*)\s*(?::[^=\n]+)?=\s*([^;\n]+)/g;
  for (const match of masked.matchAll(assignment)) {
    const rhs = match[2].trim();
    if (SUT_VALUE.test(rhs) && !INDEPENDENT_RHS.test(rhs)) {
      add(
        'self-oracle',
        match.index,
        `${match[1]} is derived from the SUT observation '${rhs.slice(0, 90)}'; expected values must be fixture/reference owned`,
      );
    }
  }
  const expectedReassign = /\b(expected[\w]*)\s*=\s*(?:Some\()?\s*([^;\n]+)/g;
  for (const match of masked.matchAll(expectedReassign)) {
    const rhs = match[2].trim();
    if (SUT_VALUE.test(rhs) && !INDEPENDENT_RHS.test(rhs) && !/^None\b/.test(rhs)) {
      add(
        'self-oracle',
        match.index,
        `${match[1]} is reassigned from the SUT observation '${rhs.slice(0, 90)}'`,
      );
    }
  }

  // (a2) an assertion that compares a SUT call with a variable previously
  // captured from the same SUT call: the test read its expectation back out
  // of the implementation instead of owning it.
  const sutAssignments = new Map();
  const sutAssignment =
    /(?:let|const|var)\s+(?:mut\s+)?([A-Za-z_][\w]*)\s*(?::[^=\n]+)?=\s*([^;\n]*(?:\.invoice\(|\.payments?\(|\.subscription\(|\.refunds?\(|\.report\(|\.outcome\(|\.score\(|\.result\(|\.settlement\(|\.receipt\()[^;\n]*);/g;
  for (const match of commentless.matchAll(sutAssignment)) {
    sutAssignments.set(match[1], normalizeCall(match[2]));
  }
  for (const [name, call] of sutAssignments) {
    if (call.length < 4 || !call.includes('(')) continue;
    const escaped = call.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const pattern = new RegExp(
      `assert(?:_eq|_ne|_deep_eq)?!\\s*\\(\\s*(?:${escaped}\\s*,\\s*&?${name}|&?${name}\\s*,\\s*${escaped})\\b`,
    );
    const found = pattern.exec(commentless);
    if (found && !isDeterminismContext(commentless, found.index)) {
      add(
        'self-oracle',
        found.index,
        `assertion compares SUT call '${call}' with its own captured result '${name}'`,
      );
    }
  }

  // (b) in-process self-attestation.
  for (const match of masked.matchAll(METRIC_SETTER)) {
    add(
      'self-attestation',
      match.index,
      `test code calls the SUT metric setter '${match[0].trim()}'; the SUT may not set its own verdict`,
    );
  }
  for (const match of masked.matchAll(SELF_VERDICT)) {
    const lineStart = masked.lastIndexOf('\n', match.index) + 1;
    const before = masked.slice(lineStart, match.index);
    const after = masked.slice(match.index, Math.min(masked.length, match.index + 80));
    if (/!\s*[\w.]*\s*$/.test(before) || /\.is_err\s*\(/.test(after)) continue;
    add(
      'self-attestation',
      match.index,
      `test code uses '${match[0].trim()}' as a self-verdict; verification must be an external oracle`,
    );
  }

  // (c) shared process/global state and unisolated data dirs.
  for (const match of masked.matchAll(/set_current_dir\s*\(/g)) {
    add('shared-state', match.index, 'test mutates the process-global current directory');
  }
  for (const match of masked.matchAll(/\bset_var\s*\(\s*["']([A-Z0-9_]+)["']/g)) {
    if (DATA_ENV.test(match[1])) {
      add(
        'shared-state',
        match.index,
        `test mutates process-global data/state env '${match[1]}'`,
      );
    }
  }
  const envMatches = [...masked.matchAll(new RegExp(DATA_ENV.source, 'g'))];
  for (const match of envMatches) {
    const windowStart = Math.max(0, match.index - 240);
    const windowEnd = Math.min(masked.length, match.index + 260);
    const window = masked.slice(windowStart, windowEnd);
    if (/(?:mkdtemp|mkdtempSync|tempdir|temp_dir|TempDir|tmpdir|join\(tmp|tempfile)/.test(window)) {
      continue;
    }
    add(
      'shared-state',
      match.index,
      'test reads a shared data-dir environment variable without a per-run temporary directory',
    );
  }

  // (d) the same function computes both sides of an assertion.
  const assertPair =
    /(?:assert_eq!|\bassert\.(?:equal|strictEqual|deepEqual|deepStrictEqual))\s*\(\s*(&?[A-Za-z_][\w:.]*(?:\([^()]*\))?)\s*,\s*(&?[A-Za-z_][\w:.]*(?:\([^()]*\))?)\s*\)/g;
  for (const match of commentless.matchAll(assertPair)) {
    const left = normalizeCall(match[1]);
    const right = normalizeCall(match[2]);
    if (
      left.length > 3 &&
      left.includes('(') &&
      left === right &&
      !isDeterminismContext(commentless, match.index)
    ) {
      add(
        'same-function-oracle',
        match.index,
        `both assertion sides call '${match[1].trim()}'; the expected value is computed by the same function`,
      );
    }
  }
  const expectedCall = /(?:let|const|var)\s+(?:mut\s+)?(expected[\w]*)\s*(?::[^=\n]+)?=\s*([A-Za-z_][\w:.]*\([^;\n]*\))\s*;/g;
  const callExpectations = [];
  for (const match of commentless.matchAll(expectedCall)) {
    callExpectations.push({ name: match[1], call: normalizeCall(match[2]), index: match.index });
  }
  for (const expectation of callExpectations) {
    const callPattern = expectation.call.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const reused = new RegExp(
      `assert(?:_eq|_ne|_deep_eq)?!\\s*\\(\\s*${callPattern}\\s*,\\s*&?${expectation.name}\\b`,
    ).exec(masked.slice(expectation.index));
    if (reused) {
      add(
        'same-function-oracle',
        expectation.index + reused.index,
        `${expectation.name} is computed by the same SUT call used as the actual value`,
      );
    }
  }

  // (e) harness/scenario/driver code compiled into the product host.
  if (/^crates\/(?:server|cli|agent|orchestrator|provider|browser|site)\/src\//.test(rel)) {
    if (/(?:harness|scenario|selftest|acceptance|oracle|test_driver|test-runner)/i.test(rel)) {
      add(
        'in-daemon-harness',
        0,
        'harness/scenario code lives in a product source tree; move it to a separate test crate/process',
      );
    }
  }

  return offenders;
}

// ------------------------------------------------------------------- report

function format(offenders) {
  return offenders
    .map((offender) => `  ${offender.file}:${offender.line} [${offender.rule}] ${offender.message}`)
    .join('\n');
}

function check() {
  let baseline;
  try {
    baseline = loadBaseline();
  } catch (error) {
    console.error(`test-separation: ${error.message}`);
    return 2;
  }
  const files = discover();
  const problems = [];
  const seen = new Set();
  for (const file of files) {
    seen.add(file.rel);
    const resolved = resolveDeclaration(file.rel, file.text, baseline);
    if (resolved.problems.length > 0) {
      for (const message of resolved.problems) {
        problems.push({ rule: 'undeclared', file: file.rel, line: 1, message });
      }
      if (resolved.declaration === undefined) continue;
    }
    problems.push(...analyze(file.rel, file.text));
  }
  for (const rel of Object.keys(baseline.files)) {
    if (!seen.has(rel)) {
      problems.push({
        rule: 'stale',
        file: rel,
        line: 1,
        message: 'baseline entry no longer names a scanned test file; remove it in a reviewed change',
      });
    }
  }
  if (problems.length > 0) {
    console.error(
      `test-separation: FAIL (${problems.length} violation(s) over ${files.length} test file(s))\n${format(problems)}`,
    );
    return 1;
  }
  console.log(
    `test-separation: PASS (${files.length} test file(s); oracle/isolation declared, no self-oracle, self-attestation, shared-state, same-function-oracle or in-daemon harness)`,
  );
  return 0;
}

// ---------------------------------------------------------------- baseline

function writeBaseline() {
  const files = discover();
  const entries = {};
  for (const file of files) {
    const inline = declarationOf(file.text);
    const declaration = inline ?? defaultOracle(file.rel);
    const problems = validDeclaration(declaration);
    if (problems.length > 0) {
      console.error(`test-separation: cannot baseline ${file.rel}: ${problems.join('; ')}`);
      return 2;
    }
    entries[file.rel] = {
      oracle: declaration.oracle,
      isolation: declaration.isolation,
      tests: testCount(file.rel, file.text),
    };
  }
  mkdirSync(dirname(BASELINE_PATH), { recursive: true });
  writeFileSync(
    BASELINE_PATH,
    `${JSON.stringify({ schema: SCHEMA, files: entries }, null, 2)}\n`,
  );
  console.log(`test-separation: baseline written for ${Object.keys(entries).length} test file(s)`);
  return 0;
}

// ----------------------------------------------------------------- selftest

function fixture(rel, text) {
  return { rel, text };
}

function analyzeFixture(name, expectedRule, rel = `tests/test-separation/fixtures/${name}`) {
  const path = join(FIXTURE_ROOT, name);
  if (!existsSync(path)) {
    console.error(`test-separation selftest: missing fixture ${name}`);
    return false;
  }
  const text = readFileSync(path, 'utf8');
  const offenders = analyze(rel, text);
  if (!offenders.some((offender) => offender.rule === expectedRule)) {
    console.error(
      `test-separation selftest: ${name} did not fire ${expectedRule}: ${JSON.stringify(offenders)}`,
    );
    return false;
  }
  return true;
}

function selftest() {
  let failures = 0;
  const expect = (name, passed) => {
    if (!passed) {
      failures += 1;
      console.error(`test-separation selftest: FAIL ${name}`);
    } else {
      console.log(`test-separation selftest: ok ${name}`);
    }
  };

  expect(
    'planted self-oracle fires',
    analyzeFixture('planted-self-oracle.rs', 'self-oracle', 'crates/payments/tests/planted_self_oracle.rs'),
  );
  expect(
    'planted self-attestation fires',
    analyzeFixture(
      'planted-self-attestation.rs',
      'self-attestation',
      'tests/supply-engineering-traps/src/planted_self_attestation.rs',
    ),
  );
  expect(
    'planted shared state fires',
    analyzeFixture('planted-shared-state.rs', 'shared-state', 'crates/agent/src/runtime/planted_shared_state_tests.rs'),
  );
  expect(
    'planted same-function oracle fires',
    analyzeFixture(
      'planted-same-function.mjs',
      'same-function-oracle',
      'apps/vscode/scripts/planted-same-function.mjs',
    ),
  );
  expect(
    'planted in-daemon harness fires',
    analyzeFixture(
      'planted-in-daemon-harness.rs',
      'in-daemon-harness',
      'crates/server/src/planted_harness_tests.rs',
    ),
  );
  expect('clean external fixture passes', readFixtureAnalyze('clean-external.rs'));

  // A new file with a valid declaration passes; the same file without one is
  // rejected by the exact resolver the --check path uses.
  const undeclared = fixture('crates/payments/tests/new_undeclared.rs', 'fn t() {}\n');
  const rejected = resolveDeclaration(undeclared.rel, undeclared.text, { files: {} });
  expect(
    'undeclared new file rejected',
    rejected.problems.some((problem) => problem.includes('no TEST-SEPARATION declaration')),
  );
  const declared = readFileSync(join(FIXTURE_ROOT, 'clean-external.rs'), 'utf8');
  const accepted = resolveDeclaration('crates/payments/tests/new_clean.rs', declared, { files: {} });
  expect(
    'clean fixture declares oracle/isolation',
    accepted.problems.length === 0 && accepted.declaration !== undefined,
  );
  const existingBaseline = {
    files: {
      'crates/payments/tests/existing.rs': {
        oracle: 'external-fixture',
        isolation: ['isolated-data-dir'],
        tests: 1,
      },
    },
  };
  const grown = resolveDeclaration(
    'crates/payments/tests/existing.rs',
    '#[test] fn one() {} #[test] fn two() {}\n',
    existingBaseline,
  );
  expect(
    'new test function in a baseline file rejected',
    grown.problems.some((problem) => problem.includes('without an inline declaration')),
  );

  if (failures > 0) {
    console.error(`test-separation selftest: FAIL (${failures} failure(s))`);
    return 1;
  }
  console.log('test-separation selftest: PASS (5 planted classes + clean declaration)');
  return 0;
}

function readFixtureAnalyze(name) {
  const text = readFileSync(join(FIXTURE_ROOT, name), 'utf8');
  const offenders = analyze(`tests/test-separation/fixtures/${name}`, text);
  return offenders.length === 0;
}

// --------------------------------------------------------------------- main

const args = process.argv.slice(2);
let code;
if (args.includes('--selftest')) {
  code = selftest();
} else if (args.includes('--write-baseline')) {
  code = writeBaseline();
} else if (args.includes('--check') || args.length === 0) {
  code = check();
} else {
  console.error('usage: node scripts/check-test-separation.mjs [--check|--selftest|--write-baseline]');
  code = 2;
}
process.exitCode = code;
