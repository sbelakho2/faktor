#!/usr/bin/env node
// Requirement registry build/validation.
//
// The registry is derived from code on every run. Committed rows are the
// requirement graph; status is DERIVED (report rule 9) from executable probes
// and lane evidence, never typed by hand. `--check` fails on:
//   * a discovered surface with no row (or a stale row with no surface),
//   * a delegation-only row (report rule 8),
//   * a malformed row (missing layers/lanes/tests/oracle/evidence kind),
//   * a row claiming `proven` without passing evidence at this commit.
// `--release` additionally requires every row proven and all five lanes green
// at one commit (release-gate.mjs enforces the same set).

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  ADVERSARIAL_BY_KIND,
  REGISTRY_SCHEMA,
  STATUS_SCHEMA,
  canonicalJson,
  isDelegationOnly,
  rowProblems,
  sortRows,
  summarizeRegistry,
} from './model.mjs';
import { discoverSurfaces } from './discovery.mjs';
import { probeCliSurface, probeScaleSchedule } from './evidence.mjs';
import { normalizeCapabilityId } from '../../scripts/discover-capabilities.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const REGISTRY_PATH = resolve(ROOT, 'tests/programme/registry.json');
export const STATUS_PATH = resolve(ROOT, 'target/certification/programme/registry-status.json');

function git(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function compactEvidence(result) {
  if (!result) return null;
  if (result.reason) return { passed: false, reason: result.reason };
  return {
    passed: result.passed === true,
    observed: result.observed ?? undefined,
    cases: result.cases ?? undefined,
  };
}

/**
 * Run the fast/dynamic probes and derive a status/evidence map keyed by row
 * id. Heavy lanes are not run here.
 */
export async function deriveStatuses(rows, { dynamic = true } = {}) {
  const byId = new Map(rows.map((row) => [row.id, row]));
  const statuses = new Map();
  const set = (id, status, evidence) => {
    if (!byId.has(id)) return;
    const current = statuses.get(id);
    const rank = { failed: 4, proven: 3, deferred: 2, stub: 1 };
    if (!current || rank[status] >= rank[current.status]) {
      statuses.set(id, { status, evidence: compactEvidence(evidence) });
    }
  };

  for (const row of rows) {
    set(row.id, row.status === 'proven' ? 'deferred' : row.status, null);
  }

  // Static act-now scans.
  try {
    const settlement = await import('./checks/settlement.mjs');
    const result = settlement.scan({ root: ROOT });
    set('act-now.settlement-static-ban', result.passed ? 'proven' : 'failed', {
      passed: result.passed,
      observed: result.observed,
      reason: result.reason,
    });
  } catch (error) {
    set('act-now.settlement-static-ban', 'failed', { passed: false, reason: String(error) });
  }
  try {
    const core = await import('./checks/core-dumps.mjs');
    const result = core.scan({ root: ROOT });
    set('act-now.core-dump-history', result.passed ? 'proven' : 'failed', {
      passed: result.passed,
      observed: result.observed,
      reason: result.reason,
    });
  } catch (error) {
    set('act-now.core-dump-history', 'failed', { passed: false, reason: String(error) });
  }
  const scale = probeScaleSchedule();
  set('act-now.scale-probe-schedule', scale.passed ? 'proven' : 'failed', scale);

  // Hostile/race generator selftests are pure and always runnable.
  try {
    const hostile = await import('./generators/hostile.mjs');
    const passed = hostile.selftest() === 0;
    set('act-now.hostile-generators', passed ? 'proven' : 'failed', { passed });
  } catch (error) {
    set('act-now.hostile-generators', 'failed', { passed: false, reason: String(error) });
  }
  try {
    const schedules = await import('./generators/schedules.mjs');
    const passed = schedules.selftest() === 0;
    set('act-now.race-schedules', passed ? 'proven' : 'failed', { passed });
  } catch (error) {
    set('act-now.race-schedules', 'failed', { passed: false, reason: String(error) });
  }

  // Executable route/CLI probes.
  if (dynamic) {
    try {
      const routes = await import('./probes/routes.mjs');
      const result = await routes.probeRoutes({ report: true });
      if (result.status === 'proven') {
        for (const measured of result.results) {
          set(normalizeCapabilityId(measured.id), 'proven', { passed: true, observed: measured.observed });
        }
      } else if (result.status === 'failed') {
        const failed = new Set(result.failures.map((entry) => normalizeCapabilityId(entry.id)));
        for (const measured of result.results) {
          if (failed.has(normalizeCapabilityId(measured.id))) {
            set(normalizeCapabilityId(measured.id), 'failed', { passed: false, observed: measured.observed });
          }
        }
      }
    } catch (error) {
      console.error(`registry: route probe unavailable: ${error.message ?? error}`);
    }

    try {
      const ordering = await import('./probes/ordering.mjs');
      const result = await ordering.probeOrdering({ report: true });
      set(
        'act-now.event-ordering',
        result.status === 'proven' ? 'proven' : result.status === 'failed' ? 'failed' : 'deferred',
        { passed: result.status === 'proven', cases: result.cases },
      );
    } catch (error) {
      set('act-now.event-ordering', 'failed', { passed: false, reason: String(error) });
    }

    try {
      const rev8 = await import('./probes/rev8.mjs');
      const result = await rev8.probeRev8({ report: true });
      for (const measured of result.results ?? []) {
        const route = normalizeCapabilityId(measured.route);
        if (measured.passed) {
          set(route, 'proven', {
            passed: true,
            cases: measured.cases,
          });
        } else {
          set(route, 'failed', {
            passed: false,
            cases: measured.cases.filter((entry) => !entry.passed),
          });
        }
      }
    } catch (error) {
      console.error(`registry: rev8 probe unavailable: ${error.message ?? error}`);
    }

    const cli = probeCliSurface();
    const cliById = new Map((cli.results ?? []).map((result) => [`cli.${result.command}`, result]));
    if (cli.status === 'proven') {
      for (const row of rows) {
        if (row.kind === 'cli-command') {
          set(row.id, 'proven', { passed: true, observed: cliById.get(row.id)?.observed });
        }
      }
    } else if (cli.status === 'failed') {
      const failed = new Set((cli.observed?.failed ?? []).map((entry) => `cli.${entry.command}`));
      for (const id of failed) set(id, 'failed', { passed: false });
    }
  }

  // Invariant unit/wiring tests exist (execution evidence comes from lanes).
  // Script-level invariants carry a `script.<ID>` id; they are deferred to
  // their named lane command, not failed for lacking a Rust function.
  for (const row of rows) {
    if (row.kind !== 'invariant' || statuses.get(row.id)?.status === 'proven') continue;
    const { findRustFn } = await import('./evidence.mjs');
    const rustIds = row.test_ids.filter((id) => !id.startsWith('script.'));
    if (rustIds.length === 0) {
      set(row.id, 'deferred', null);
      continue;
    }
    const found = rustIds.some((id) => findRustFn(id) !== null);
    set(row.id, found ? 'deferred' : 'failed', found ? null : { passed: false, reason: 'test fn missing' });
  }

  // Row-declared stubs stay stubs; rows whose tests are only planned stay
  // deferred. No status is promoted without evidence.
  for (const row of rows) {
    if (!statuses.has(row.id)) {
      set(row.id, row.status === 'stub' ? 'stub' : 'deferred', null);
    }
  }
  return statuses;
}

export async function buildRegistry({ dynamic = true, root = ROOT } = {}) {
  void root;
  const discovered = discoverSurfaces();
  const statuses = await deriveStatuses(discovered, { dynamic });
  const rows = sortRows(
    discovered.map((row) => {
      const derived = statuses.get(row.id) ?? { status: 'stub', evidence: null };
      return {
        ...row,
        adversarial: row.adversarial ?? ADVERSARIAL_BY_KIND[row.kind] ?? ['hostile-input'],
        status: derived.status,
        evidence: derived.evidence,
      };
    }),
  );
  const summary = summarizeRegistry({ rows });
  return {
    schema: REGISTRY_SCHEMA,
    generated_at: new Date().toISOString(),
    commit: git(['rev-parse', 'HEAD']),
    tree: git(['rev-parse', 'HEAD^{tree}']),
    layers: ['structural', 'unit', 'property', 'fault', 'integration', 'live'],
    lanes: ['pull', 'trusted', 'nightly', 'weekly-live', 'release'],
    counts: summary,
    rows,
  };
}

export function stableRows(registry) {
  return (registry.rows ?? []).map((row) => ({
    id: row.id,
    kind: row.kind,
    path: row.path,
    layers: row.layers,
    lanes: row.lanes,
    test_ids: row.test_ids,
    evidence_kind: row.evidence_kind,
    oracle: row.oracle,
    adversarial: row.adversarial,
  }));
}

export function validateRegistry(registry, { discovered = null, release = false } = {}) {
  const problems = [];
  const rows = Array.isArray(registry?.rows) ? registry.rows : [];
  if (rows.length === 0) problems.push('registry: no rows');
  const ids = new Set();
  for (const row of rows) {
    for (const problem of rowProblems(row)) problems.push(problem);
    if (ids.has(row.id)) problems.push(`duplicate row: ${row.id}`);
    ids.add(row.id);
    const rowPath = String(row.path ?? '').split('#')[0];
    if (rowPath && !existsSync(join(ROOT, rowPath))) {
      problems.push(`missing surface path: ${row.id} -> ${row.path}`);
    }
  }
  const surfaces = discovered ?? discoverSurfaces();
  const surfaceIds = new Set(surfaces.map((row) => row.id));
  for (const id of surfaceIds) {
    if (!ids.has(id)) problems.push(`uncovered surface: ${id}`);
  }
  for (const id of ids) {
    if (!surfaceIds.has(id)) problems.push(`stale row (no discovered surface): ${id}`);
  }
  if (release) {
    for (const row of rows) {
      if (row.status !== 'proven') {
        problems.push(`release requires a passing proof: ${row.id} is ${row.status}`);
      }
    }
  }
  return problems;
}

/** Compare committed registry stable fields with a fresh discovery. */
export function driftProblems(committed, discovered = discoverSurfaces()) {
  const problems = [];
  const expected = new Map(discovered.map((row) => [row.id, row]));
  const actual = new Map((committed.rows ?? []).map((row) => [row.id, row]));
  for (const [id, row] of expected) {
    const have = actual.get(id);
    if (!have) {
      problems.push(`registry drift: ${id} is discovered but absent from the committed registry (run build-registry.mjs)`);
      continue;
    }
    for (const field of ['kind', 'path', 'layers', 'lanes', 'test_ids', 'evidence_kind', 'oracle', 'adversarial']) {
      const left = canonicalJson(have[field]);
      const right = canonicalJson(row[field]);
      if (left !== right) {
        problems.push(`registry drift: ${id}.${field} differs from discovery (${left} != ${right})`);
      }
    }
  }
  for (const id of actual.keys()) {
    if (!expected.has(id)) {
      problems.push(`registry drift: ${id} is committed but no longer discovered`);
    }
  }
  return problems;
}

export function writeRegistry(registry) {
  mkdirSync(dirname(REGISTRY_PATH), { recursive: true });
  writeFileSync(REGISTRY_PATH, `${JSON.stringify(registry, null, 2)}\n`);
  mkdirSync(dirname(STATUS_PATH), { recursive: true });
  const status = {
    schema: STATUS_SCHEMA,
    generated_at: registry.generated_at,
    commit: registry.commit,
    counts: registry.counts,
    rows: registry.rows.map((row) => ({
      id: row.id,
      status: row.status,
      evidence: row.evidence,
      layers: row.layers,
      lanes: row.lanes,
      test_ids: row.test_ids,
    })),
  };
  writeFileSync(STATUS_PATH, `${JSON.stringify(status, null, 2)}\n`);
  return { registry: REGISTRY_PATH, status: STATUS_PATH, digest: sha(JSON.stringify(registry.rows)) };
}

export function loadCommitted() {
  if (!existsSync(REGISTRY_PATH)) return null;
  try {
    return JSON.parse(readFileSync(REGISTRY_PATH, 'utf8'));
  } catch {
    return null;
  }
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`registry selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`registry selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const row = (id, extra = {}) => ({
    id,
    kind: 'native-route',
    path: 'crates/server/src/api/lifecycle.rs',
    layers: ['contracts'],
    lanes: ['pull', 'trusted'],
    test_ids: [`native.route-contract.${id}`],
    evidence_kind: 'exact-value',
    oracle: 'authenticated malformed input is refused typed, never 5xx',
    adversarial: ['auth', 'strict-body'],
    status: 'proven',
    evidence: { passed: true },
    ...extra,
  });
  const discovered = [row('native.get.native.one'), row('native.post.native.two')];
  const good = { rows: discovered.map((entry) => ({ ...entry })) };
  check('a complete registry validates', validateRegistry(good, { discovered }).length === 0);
  const missing = { rows: [discovered[0]] };
  check(
    'a missing surface is uncovered',
    validateRegistry(missing, { discovered }).some((problem) => problem.startsWith('uncovered surface:')),
  );
  const stale = { rows: [...discovered, row('native.get.native.gone')] };
  check(
    'a stale row is rejected',
    validateRegistry(stale, { discovered }).some((problem) => problem.includes('stale row')),
  );
  const delegated = {
    rows: [
      row('native.get.native.one', { test_ids: ['INV-CLIENT-ROUTE-CONTRACT'] }),
      discovered[1],
    ],
  };
  check(
    'delegation-only rows fail',
    validateRegistry(delegated, { discovered }).some((problem) => problem.includes('delegation-only')),
  );
  const fake = { rows: discovered.map((entry) => ({ ...entry, evidence: null })) };
  check(
    'proven without evidence fails',
    validateRegistry(fake, { discovered }).some((problem) => problem.includes('no passing evidence')),
  );
  const release = { rows: discovered.map((entry) => ({ ...entry, status: 'deferred' })) };
  check(
    'release refuses deferred rows',
    validateRegistry(release, { discovered, release: true }).some((problem) => problem.startsWith('release requires')),
  );
  const wrongLayer = { rows: [row('native.get.native.one', { lanes: ['pull'], layers: ['hardware'] }), discovered[1]] };
  check(
    'a layer not run by its lane fails',
    validateRegistry(wrongLayer, { discovered }).some((problem) => problem.includes('not run by any assigned lane')),
  );
  const real = validateRegistry(
    { rows: discoverSurfaces().map((entry) => ({ ...entry, status: 'deferred' })) },
    { discovered: discoverSurfaces() },
  );
  check(`real discovery validates (${real.length} problems)`, real.length === 0, real.slice(0, 3).join('; '));
  if (failures > 0) {
    console.error(`registry selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('registry selftest: PASS');
  return 0;
}

export async function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) return selftest();
  const mode = args.includes('--release') ? 'release' : 'check';
  const dynamic = !args.includes('--no-probe');
  if (args.includes('--check')) {
    const committed = loadCommitted();
    const discovered = discoverSurfaces();
    const problems = [];
    if (committed === null) problems.push('registry: tests/programme/registry.json is missing');
    else {
      problems.push(...driftProblems(committed, discovered));
      problems.push(...validateRegistry(committed, { discovered, release: mode === 'release' }));
    }
    if (dynamic) {
      const fresh = await buildRegistry({ dynamic: true });
      problems.push(...validateRegistry(fresh, { discovered, release: mode === 'release' }));
      writeRegistry(fresh);
      for (const [kind, count] of Object.entries(fresh.counts.by_status)) {
        console.log(`registry: status ${kind}: ${count}`);
      }
    }
    if (problems.length > 0) {
      for (const problem of problems.slice(0, 80)) console.error(`registry: ${problem}`);
      console.error(
        `registry ${mode}: FAIL (${problems.length} problem(s))`,
      );
      return 1;
    }
    console.log(`registry ${mode}: PASS (${discovered.length} surfaces)`);
    return 0;
  }
  const registry = await buildRegistry({ dynamic });
  const written = writeRegistry(registry);
  for (const [kind, count] of Object.entries(registry.counts.by_kind)) {
    console.log(`registry: kind ${kind}: ${count}`);
  }
  for (const [status, count] of Object.entries(registry.counts.by_status)) {
    console.log(`registry: status ${status}: ${count}`);
  }
  console.log(`registry: wrote ${written.registry} (${registry.rows.length} rows)`);
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().then((code) => {
    process.exitCode = code;
  });
}
