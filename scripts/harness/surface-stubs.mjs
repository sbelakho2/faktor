#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=pure-unit
//
// HAR-01 surface stub generator + completeness gate.
//
// Consumes the mechanically discovered capability set
// (`scripts/discover-capabilities.mjs`, READ-only import) and generates one
// minimal stub definition per discovered surface x template case, using the
// fake-provider surface vocabulary: `crates/provider/src/lib.rs`
// (`ScriptedResponse::{Text,Reasoning,ToolCall,End,Die}` + `inject_rate_limit`)
// and the loopback HTTP fakes (`scripts/edition8/fake-openai.mjs`,
// `scripts/edition8/fake-ollama.mjs` frame shapes). Nothing here executes a
// stub; the stub set is the coverage definition a runner later drives, and a
// still-failing/missing stub blocks release via the gate.
//
// Honesty rule: surfaces the in-tree loopback fakes cannot express (IDE
// contribution surfaces, provider wire formats with no fake) are marked
// `excluded` with a machine-checked reason code. An exclusion without a
// reason, or an exclusion of a surface the fakes CAN express, fails the gate.
//
// Modes:
//   node scripts/harness/surface-stubs.mjs generate [--out <path>]
//   node scripts/harness/surface-stubs.mjs gate [--stubs <path>]
//   node scripts/harness/surface-stubs.mjs selftest
//
// Exit codes: 0 pass; 1 gate failure; 2 usage error.

import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discover } from '../discover-capabilities.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const DEFAULT_STUBS = resolve(HERE, 'surface-stubs.generated.json');

export const SCHEMA = 'faktor.harness.surface-stub/v1';

/** The fixed template cases every surface is stubbed for. */
export const TEMPLATE_CASES = Object.freeze([
  'happy-path',
  'adversarial-input',
  'fault-mid-stream',
  'rate-limit',
  'illegal-state',
]);

const SOURCES = Object.freeze({
  'native-route': { transport: 'http' },
  'builtin-tool': { transport: 'in-process' },
  'cli-command': { transport: 'stdio' },
  'provider-kind': { transport: 'http' },
});

const CLASSES = new Set(['chat', 'stream', 'tool', 'other']);
const STATUSES = new Set(['stubbed', 'excluded']);
const TRANSPORTS = new Set(['http', 'stdio', 'in-process', 'ide-host']);
const SCRIPT_OPS = new Set(['text', 'reasoning', 'tool-call', 'end', 'die', 'rate-limit', 'typed-error']);

/**
 * Reason codes an exclusion may carry. Every excluded surface maps to exactly
 * one code; the gate recomputes the map and a mismatch fails.
 */
export const EXCLUSION_CODES = Object.freeze({
  'ide-host-required':
    'IDE contribution surface (command/view/configuration/menu/action/tool-window): it only exists inside a real Extension Host / EDT frame; a loopback fake provider has no rendering surface, so no stub the fakes can express exists for it.',
  'wire-format-not-expressible':
    'Provider kind whose request/response wire format is implemented by no in-tree loopback fake (fake-openai.mjs implements the OpenAI-compatible wire, fake-ollama.mjs the Ollama-native wire); a stub would claim wire coverage the fakes cannot express.',
  'routing-config-not-endpoint':
    'Provider kind is a routing/profile configuration family, not an invokable endpoint; there is no standalone wire surface for a fake to serve.',
});

/**
 * Provider kinds the in-tree loopback fakes CAN express (OpenAI-compatible or
 * Ollama-native wire). Everything else under provider.* is excluded.
 */
const FAKABLE_PROVIDER_KINDS = new Set(['provider.open-ai', 'provider.deepseek', 'provider.ollama']);

/** gateway is a routing configuration family, not a wire endpoint. */
const ROUTING_PROVIDER_KINDS = new Set(['provider.gateway']);

function sha256(text) {
  return createHash('sha256').update(text).digest('hex');
}

/** Deterministic fixture text: derived from (id, case, salt), never random. */
function derived(id, kase, salt) {
  const digest = sha256(`faktor-h1:${id}:${kase}:${salt}`).slice(0, 16);
  return `stub-${salt}-${digest}`;
}

/** Surface class from the id shape (chat/stream/tool/other). */
export function classify(id) {
  if (id.startsWith('tool.')) return 'tool';
  if (id.startsWith('native.')) {
    if (/stream|\.events|live\.output|live\.snapshot/.test(id)) return 'stream';
    if (/chat|prompt|drive|message|completion/.test(id)) return 'chat';
    return 'other';
  }
  if (id.startsWith('provider.')) return 'chat';
  return 'other';
}

function sourceOf(id, sources) {
  const declared = sources[id];
  if (declared && SOURCES[declared]) return declared;
  if (id.startsWith('vscode.')) return 'vscode-contribution';
  if (id.startsWith('jetbrains.')) return 'jetbrains-contribution';
  return declared ?? 'unknown';
}

/**
 * Exclusion decision for a surface. Returns { excluded, code } — code is set
 * only when excluded. Surfaces the fakes CAN express must come back
 * non-excluded; the gate asserts both directions.
 */
export function exclusionFor(id) {
  if (id.startsWith('vscode.') || id.startsWith('jetbrains.')) {
    return { excluded: true, code: 'ide-host-required' };
  }
  if (id.startsWith('provider.')) {
    if (FAKABLE_PROVIDER_KINDS.has(id)) return { excluded: false, code: null };
    if (ROUTING_PROVIDER_KINDS.has(id)) return { excluded: true, code: 'routing-config-not-endpoint' };
    return { excluded: true, code: 'wire-format-not-expressible' };
  }
  return { excluded: false, code: null };
}

/** HTTP probe path for a native route / provider wire id (best-effort shape). */
function probePath(id) {
  if (id.startsWith('native.')) {
    return `/${id
      .slice('native.'.length)
      .replace(/^(get|post|put|patch|delete)\./, '')
      .replace(/\./g, '/')
      .replace(/\/id$/, '/{id}')}`;
  }
  return '/v1/chat/completions';
}

/**
 * The scripted fake behavior + probe plan for one (surface, case). Mirrors
 * the ScriptedResponse vocabulary and the loopback fakes' frame shapes.
 */
function stubScript(id, kase, klass, transport) {
  const streaming = klass === 'stream' || klass === 'chat';
  switch (kase) {
    case 'happy-path':
      return {
        script: [
          { op: 'text', text: derived(id, kase, 'answer') },
          ...(streaming || transport === 'in-process' ? [{ op: 'end' }] : []),
        ],
        probe:
          transport === 'http'
            ? { method: 'POST', path: probePath(id), expect: { status: 200, firstEvent: 'text' } }
            : transport === 'stdio'
              ? { method: 'spawn', args: ['--version'], expect: { exit: 0 } }
              : { method: 'call', tool: id.slice(5), expect: { ok: true, firstEvent: 'text' } },
      };
    case 'adversarial-input':
      return {
        script: [{ op: 'typed-error', status: 400, kind: 'malformed' }],
        probe:
          transport === 'http'
            ? {
                method: 'POST',
                path: probePath(id),
                payload: 'malformed-utf8-�-oversized',
                expect: { status: 400, errorKind: 'malformed' },
              }
            : transport === 'stdio'
              ? { method: 'spawn', args: ['--not-a-real-flag-9f3a'], expect: { exitNonZero: true } }
              : { method: 'call', tool: id.slice(5), payload: null, expect: { ok: false, errorKind: 'malformed' } },
      };
    case 'fault-mid-stream':
      return {
        script: [
          { op: 'text', text: derived(id, kase, 'partial') },
          { op: 'die', kind: 'connection-reset' },
        ],
        probe:
          transport === 'http'
            ? { method: 'POST', path: probePath(id), stream: true, expect: { diesAfterFrames: 1 } }
            : transport === 'stdio'
              ? { method: 'spawn', env: { FAKTOR_STUB_FAULT: '1' }, expect: { exitNonZero: true } }
              : { method: 'call', tool: id.slice(5), expect: { diesAfterEvents: 1 } },
      };
    case 'rate-limit':
      return {
        script: [{ op: 'rate-limit', retryAfterS: 1 }],
        probe:
          transport === 'http'
            ? { method: 'POST', path: probePath(id), expect: { status: 429, retryAfterS: 1 } }
            : transport === 'stdio'
              ? { method: 'spawn', env: { FAKTOR_STUB_RATE_LIMIT: '1' }, expect: { exit: 75 } }
              : { method: 'call', tool: id.slice(5), expect: { ok: false, errorKind: 'rate-limited' } },
      };
    case 'illegal-state':
      return {
        script: [
          { op: 'text', text: derived(id, kase, 'first') },
          { op: 'typed-error', status: 409, kind: 'illegal-state' },
        ],
        probe:
          transport === 'http'
            ? {
                method: 'POST',
                path: probePath(id),
                invoke: 'duplicate',
                expect: { status: 409, errorKind: 'illegal-state' },
              }
            : transport === 'stdio'
              ? { method: 'spawn', args: ['--help'], expect: { exit: 0 } }
              : {
                  method: 'call',
                  tool: id.slice(5),
                  invoke: 'duplicate',
                  expect: { ok: false, errorKind: 'illegal-state' },
                },
      };
    default:
      throw new Error(`unknown template case ${kase}`);
  }
}

/** Build one stub definition entry. */
function buildStub(id, kase, sources) {
  const source = sourceOf(id, sources);
  const klass = classify(id);
  const transport =
    source === 'vscode-contribution' || source === 'jetbrains-contribution'
      ? 'ide-host'
      : (SOURCES[source]?.transport ?? 'http');
  const decision = exclusionFor(id);
  if (decision.excluded) {
    return {
      id,
      case: kase,
      source,
      class: klass,
      transport,
      status: 'excluded',
      exclusion: { code: decision.code, reason: EXCLUSION_CODES[decision.code] },
    };
  }
  const { script, probe } = stubScript(id, kase, klass, transport);
  return {
    id,
    case: kase,
    source,
    class: klass,
    transport,
    status: 'stubbed',
    script,
    probe,
    isolation: transport === 'stdio' ? ['isolated-data-dir'] : [],
  };
}

/** Deterministic full stub set for the current discovery. */
export function generateStubs(root = ROOT) {
  const { ids, sources } = discover(root);
  const stubs = [];
  for (const id of ids) {
    for (const kase of TEMPLATE_CASES) {
      stubs.push(buildStub(id, kase, sources));
    }
  }
  return { schema: SCHEMA, surfaces: ids.length, cases: TEMPLATE_CASES.length, stubs };
}

function stableStringify(value) {
  return JSON.stringify(value, null, 2);
}

/**
 * Validate a stub set against the freshly discovered surface set.
 * Returns { failures: string[], stats } — an empty failures array is a pass.
 */
export function validate(stubSet, discovered) {
  const failures = [];
  const ids = new Set(discovered.ids);
  const seen = new Map();
  for (const stub of stubSet?.stubs ?? []) {
    const key = `${stub.id}\u0000${stub.case}`;
    if (seen.has(key)) failures.push(`duplicate stub for ${stub.id} x ${stub.case}`);
    seen.set(key, stub);
  }
  let stubbed = 0;
  let excluded = 0;
  for (const id of ids) {
    for (const kase of TEMPLATE_CASES) {
      const stub = seen.get(`${id}\u0000${kase}`);
      if (!stub) {
        failures.push(`missing stub for ${id} x ${kase}`);
        continue;
      }
      const decision = exclusionFor(id);
      if (decision.excluded) {
        excluded += 1;
        if (stub.status !== 'excluded') {
          failures.push(`${id} x ${kase}: fakes cannot express this surface but stub status is ${stub.status}`);
          continue;
        }
        if (!stub.exclusion || typeof stub.exclusion.reason !== 'string' || stub.exclusion.reason.trim().length < 20) {
          failures.push(`${id} x ${kase}: exclusion without a reason`);
        } else if (stub.exclusion.code !== decision.code) {
          failures.push(`${id} x ${kase}: exclusion code ${stub.exclusion.code} != expected ${decision.code}`);
        } else if (stub.exclusion.reason !== EXCLUSION_CODES[decision.code]) {
          failures.push(`${id} x ${kase}: exclusion reason text diverges from the approved reason`);
        }
      } else {
        stubbed += 1;
        if (stub.status !== 'stubbed') {
          failures.push(`${id} x ${kase}: stubbable surface marked ${stub.status}`);
          continue;
        }
        if (stub.exclusion) failures.push(`${id} x ${kase}: stub carries an exclusion but is stubbable`);
        failures.push(...validateShape(stub, id, kase, discovered.sources));
      }
    }
  }
  for (const [key, stub] of seen) {
    const [id, kase] = key.split('\u0000');
    if (!ids.has(id)) failures.push(`orphan stub for unknown surface ${id} x ${kase}`);
    else if (!TEMPLATE_CASES.includes(kase)) failures.push(`orphan template case ${kase} for ${id}`);
  }
  const expected = ids.size * TEMPLATE_CASES.length;
  if ((stubSet?.stubs?.length ?? 0) !== expected) {
    failures.push(`stub count ${stubSet?.stubs?.length ?? 0} != surfaces ${ids.size} x cases ${TEMPLATE_CASES.length} (${expected})`);
  }
  return {
    failures,
    stats: { surfaces: ids.size, cases: TEMPLATE_CASES.length, stubbed, excluded, pairs: expected },
  };
}

function validateShape(stub, id, kase, sources) {
  const failures = [];
  const fail = (msg) => failures.push(`${id} x ${kase}: ${msg}`);
  if (stub.schema !== undefined && stub.schema !== SCHEMA) fail(`bad schema ${stub.schema}`);
  if (stub.id !== id) fail(`id mismatch ${stub.id}`);
  if (stub.case !== kase) fail(`case mismatch ${stub.case}`);
  if (!CLASSES.has(stub.class)) fail(`bad class ${stub.class}`);
  else if (stub.class !== classify(id)) fail(`class ${stub.class} != derived ${classify(id)}`);
  if (!TRANSPORTS.has(stub.transport)) fail(`bad transport ${stub.transport}`);
  if (!STATUSES.has(stub.status)) fail(`bad status ${stub.status}`);
  if (!Array.isArray(stub.script) || stub.script.length === 0) fail('empty script');
  else {
    for (const step of stub.script) {
      if (!SCRIPT_OPS.has(step.op)) fail(`unknown script op ${step.op}`);
      if (step.op === 'text' && (typeof step.text !== 'string' || !step.text.startsWith('stub-'))) {
        fail('text step is not deterministically derived');
      }
      if (step.op === 'rate-limit' && step.retryAfterS !== 1) fail('rate-limit step must carry retryAfterS: 1');
      if (step.op === 'die' && typeof step.kind !== 'string') fail('die step without kind');
      if (step.op === 'typed-error' && !Number.isInteger(step.status)) fail('typed-error step without status');
    }
  }
  if (!stub.probe || typeof stub.probe.method !== 'string') fail('probe without method');
  if (!Array.isArray(stub.isolation)) fail('isolation must be a list');
  if (sourceOf(id, sources) === 'cli-command' && stub.transport !== 'stdio') fail('cli surface must be stdio');
  return failures;
}

function main() {
  const args = process.argv.slice(2);
  const mode = args[0] ?? 'gate';
  const value = (name, fallback) => {
    const at = args.indexOf(`--${name}`);
    return at >= 0 && at + 1 < args.length ? args[at + 1] : fallback;
  };

  if (mode === 'generate') {
    const set = generateStubs(ROOT);
    const out = value('out', null);
    const text = `${stableStringify(set)}\n`;
    if (out) {
      const path = resolve(out);
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, text);
      console.log(`surface-stubs: wrote ${set.stubs.length} stubs (${set.surfaces} surfaces x ${set.cases} cases) -> ${out}`);
    } else {
      process.stdout.write(text);
    }
    return 0;
  }

  if (mode === 'gate') {
    const fresh = generateStubs(ROOT);
    const discovered = discover(ROOT);
    const stubPath = value('stubs', DEFAULT_STUBS);
    let set = fresh;
    const drift = [];
    if (stubPath && existsSync(stubPath)) {
      const fileSet = JSON.parse(readFileSync(stubPath, 'utf8'));
      set = fileSet;
      if (stableStringify(fresh) !== stableStringify(fileSet)) {
        drift.push(`stub file ${stubPath} diverges from fresh generation (discovery changed or file stale); re-run generate --out`);
      }
    }
    const { failures, stats } = validate(set, discovered);
    const all = [...drift, ...failures];
    if (all.length > 0) {
      for (const failure of all.slice(0, 40)) console.error(`surface-stubs gate FAIL: ${failure}`);
      if (all.length > 40) console.error(`surface-stubs gate FAIL: … ${all.length - 40} more`);
      console.error(`surface-stubs gate: FAIL (${all.length} problems)`);
      return 1;
    }
    console.log(
      `surface-stubs gate: PASS — ${stats.surfaces} surfaces x ${stats.cases} cases = ${stats.pairs} pairs; ` +
        `${stats.stubbed} stubbed, ${stats.excluded} excluded (reasoned)`,
    );
    return 0;
  }

  if (mode === 'selftest') {
    return selftest();
  }

  console.error('usage: node scripts/harness/surface-stubs.mjs [generate [--out path]|gate [--stubs path]|selftest]');
  return 2;
}

function selftest() {
  let failures = 0;
  const check = (name, ok, detail = '') => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}${detail ? ` — ${detail}` : ''}`);
      failures += 1;
    }
  };
  const fresh = generateStubs(ROOT);
  const discovered = discover(ROOT);

  // Determinism: two generations are byte-identical.
  const again = generateStubs(ROOT);
  check('generation is deterministic', stableStringify(fresh) === stableStringify(again));

  // The pristine set passes.
  const base = validate(fresh, discovered);
  check('pristine stub set passes the gate', base.failures.length === 0, base.failures.slice(0, 3).join('; '));

  const clone = () => JSON.parse(stableStringify(fresh));
  const expectFail = (name, mutate, needle) => {
    const set = clone();
    mutate(set);
    const { failures: fails } = validate(set, discovered);
    check(
      name,
      fails.some((f) => f.includes(needle)),
      `expected failure containing ${JSON.stringify(needle)}; got ${fails.slice(0, 3).join('; ') || 'none'}`,
    );
  };

  // Adversarial: a missing pair must fail (the still-failing stub blocker).
  expectFail(
    'a deleted stub pair fails the gate',
    (set) => {
      const at = set.stubs.findIndex((s) => s.status === 'stubbed');
      set.stubs.splice(at, 1);
    },
    'missing stub',
  );

  // Adversarial: an orphan stub (unknown surface) must fail.
  expectFail(
    'an orphan stub fails the gate',
    (set) => {
      set.stubs.push({ ...set.stubs[0], id: 'native.get.ghost-surface-never-discovered' });
    },
    'orphan stub',
  );

  // Adversarial: an exclusion without a reason must fail.
  expectFail(
    'an exclusion without a reason fails the gate',
    (set) => {
      const excluded = set.stubs.find((s) => s.status === 'excluded');
      excluded.exclusion = { code: excluded.exclusion.code, reason: '' };
    },
    'exclusion without a reason',
  );

  // Adversarial: excluding a stubbable surface must fail.
  expectFail(
    'excluding a stubbable surface fails the gate',
    (set) => {
      const stub = set.stubs.find((s) => s.status === 'stubbed');
      stub.status = 'excluded';
      stub.exclusion = { code: 'ide-host-required', reason: EXCLUSION_CODES['ide-host-required'] };
    },
    'stubbable surface marked excluded',
  );

  // Adversarial: un-excluding a surface the fakes cannot express must fail.
  expectFail(
    'claiming a stub for an IDE surface fails the gate',
    (set) => {
      const stub = set.stubs.find((s) => s.status === 'excluded' && s.id.startsWith('vscode.'));
      delete stub.exclusion;
      stub.status = 'stubbed';
      stub.script = [{ op: 'end' }];
      stub.probe = { method: 'call', tool: 'x', expect: {} };
      stub.isolation = [];
    },
    'cannot express',
  );

  // Adversarial: a corrupted script op must fail shape validation.
  expectFail(
    'an unknown script op fails shape validation',
    (set) => {
      const stub = set.stubs.find((s) => s.status === 'stubbed' && s.script?.length > 0);
      stub.script[0] = { op: 'explode-universe' };
    },
    'unknown script op',
  );

  // Adversarial: class forgery must fail.
  expectFail(
    'a forged surface class fails shape validation',
    (set) => {
      const stub = set.stubs.find((s) => s.id.startsWith('tool.') && s.status === 'stubbed');
      stub.class = 'chat';
    },
    'class chat != derived tool',
  );

  // Adversarial: duplicate pair must fail.
  expectFail(
    'a duplicated stub pair fails the gate',
    (set) => {
      set.stubs.push({ ...set.stubs.find((s) => s.status === 'stubbed') });
    },
    'duplicate stub',
  );

  // Coverage honesty: exclusion set is exactly IDE + non-fakable providers.
  const excludedIds = new Set(fresh.stubs.filter((s) => s.status === 'excluded').map((s) => s.id));
  const expectedExcluded = discovered.ids.filter((id) => exclusionFor(id).excluded);
  check(
    'exclusion set matches the classifier exactly',
    excludedIds.size === expectedExcluded.length && expectedExcluded.every((id) => excludedIds.has(id)),
    `${excludedIds.size} excluded vs ${expectedExcluded.length} expected`,
  );
  check(
    'no vscode/jetbrains/provider-wire surface claims a stub',
    [...excludedIds].some((id) => id.startsWith('vscode.')) && [...excludedIds].some((id) => id.startsWith('provider.')),
  );
  check(
    'fakable provider kinds are stubbed (open-ai/deepseek/ollama)',
    ['provider.open-ai', 'provider.deepseek', 'provider.ollama'].every(
      (id) => !excludedIds.has(id) && fresh.stubs.some((s) => s.id === id && s.status === 'stubbed'),
    ),
  );

  if (failures > 0) {
    console.error(`surface-stubs selftest: FAIL (${failures})`);
    return 1;
  }
  console.log('surface-stubs selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1]);
if (isMain) {
  process.exit(main());
}
