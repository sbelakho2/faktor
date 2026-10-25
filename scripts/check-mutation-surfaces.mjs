#!/usr/bin/env node
// Per-surface planted-mutation gate.
//
// The invariant registry proves each named invariant has a mutation, but it
// does not prove that every PRODUCT SURFACE (payments, compaction, agent
// communication, task intelligence, security, UI wiring, performance bounds,
// routes, tools/providers, programme registry, core runtime) is covered by at
// least one planted mutation. This gate closes that hole mechanically:
//
//   1. tests/mutation-surfaces.json enumerates every covered surface and the
//      mutation specs that belong to it (explicit ids and/or id prefixes).
//   2. every spec under scripts/mutations/ must resolve to EXACTLY ONE
//      surface; a new spec in an unknown family fails until the manifest is
//      updated, so new surfaces cannot silently ship without mutations.
//   3. every spec must name an exact single-line gate and a non-empty
//      expected failure signature (`expect`); a compile error, an environment
//      failure, or any other failure cannot masquerade as detection.
//   4. every registered invariant must have a mutation_command that runs the
//      spec with the same id, and an invariant without a spec fails.
//   5. every programme-registry row kind must be claimed by a surface, and
//      every row whose test ids name an invariant must name a registered
//      invariant with a mutation. A new surface/row kind or a new invariant
//      without a mutation turns the gate red.
//   6. `--campaign <file>` consumes the runner's machine-readable evidence and
//      fails on any survivor, environment failure, or unrun spec.
//
// Usage:
//   node scripts/check-mutation-surfaces.mjs [--check]
//   node scripts/check-mutation-surfaces.mjs --campaign target/certification/mutation-campaign.json
//   node scripts/check-mutation-surfaces.mjs --selftest
//
// Exit codes: 0 pass; 1 violations; 2 usage.

import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const SCHEMA = 'faktor-mutation-surfaces/v1';
const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

function argValue(args, name, fallback = '') {
  const index = args.indexOf(name);
  if (index === -1) return fallback;
  const value = args[index + 1];
  if (value === undefined || value.startsWith('--')) return '';
  return value;
}

export function parseInvariants(text) {
  const invariants = new Map();
  for (const block of text.split('[[invariant]]').slice(1)) {
    const id = /^id\s*=\s*"(.*)"$/m.exec(block);
    if (id === null) continue;
    const mutation = /^mutation_command\s*=\s*"(.*)"$/m.exec(block);
    const critical = /^release_critical\s*=\s*(true|false)$/m.exec(block);
    invariants.set(id[1], {
      id: id[1],
      mutationCommand: mutation === null ? '' : mutation[1],
      releaseCritical: critical === null ? true : critical[1] === 'true',
    });
  }
  return invariants;
}

export function loadWorld(root = ROOT) {
  const manifestPath = join(root, 'tests/mutation-surfaces.json');
  if (!existsSync(manifestPath)) {
    throw new Error(`${manifestPath} does not exist`);
  }
  const manifest = readJson(manifestPath);
  const specs = new Map();
  const specDir = join(root, 'scripts/mutations');
  if (existsSync(specDir)) {
    for (const file of readdirSync(specDir).sort()) {
      if (!file.endsWith('.json')) continue;
      const document = readJson(join(specDir, file));
      if (typeof document.id === 'string') {
        specs.set(document.id, { ...document, __file: file });
      }
    }
  }
  const invariants = parseInvariants(readFileSync(join(root, 'tests/invariants.toml'), 'utf8'));
  const registry = readJson(join(root, 'tests/programme/registry.json'));
  return { root, manifest, specs, invariants, rows: Array.isArray(registry.rows) ? registry.rows : [] };
}

const SPEC_ID = /^INV-[A-Z0-9-]+$/;

function checkSpec(spec, surfaceIds) {
  const problems = [];
  const at = spec.__file ?? spec.id ?? '<spec>';
  if (typeof spec.id !== 'string' || !SPEC_ID.test(spec.id)) {
    problems.push(`${at}: id ${JSON.stringify(spec.id)} is not an INV-* id`);
  }
  if (typeof spec.gate !== 'string' || spec.gate.trim() === '') {
    problems.push(`${at}: missing non-empty 'gate'`);
  } else {
    if (spec.gate.includes('\n')) problems.push(`${at}: gate must be a single command line`);
    if (spec.gate !== spec.gate.trim()) problemPushed(problems, `${at}: gate has surrounding whitespace`);
  }
  if (typeof spec.expect !== 'string' || spec.expect.trim() === '') {
    problems.push(`${at}: missing expected failure signature 'expect'`);
  } else {
    try {
      new RegExp(spec.expect);
    } catch (error) {
      problems.push(`${at}: 'expect' is not a valid regex: ${error.message}`);
    }
  }
  if (spec.surface !== undefined) {
    if (typeof spec.surface !== 'string' || !surfaceIds.has(spec.surface)) {
      problems.push(`${at}: surface ${JSON.stringify(spec.surface)} is not declared in tests/mutation-surfaces.json`);
    }
  }
  return problems;
}

function problemPushed(problems, message) {
  if (!problems.includes(message)) problems.push(message);
}

function surfacesOf(world) {
  const surfaces = Array.isArray(world.manifest.surfaces) ? world.manifest.surfaces : [];
  const ids = new Set();
  const problems = [];
  for (const surface of surfaces) {
    if (typeof surface.id !== 'string' || surface.id.trim() === '') {
      problems.push('manifest: a surface has no id');
      continue;
    }
    if (ids.has(surface.id)) problems.push(`manifest: duplicate surface ${surface.id}`);
    ids.add(surface.id);
    if (!Array.isArray(surface.specs) && !Array.isArray(surface.spec_prefixes) && surface.all_invariants !== true) {
      problems.push(`manifest: surface ${surface.id} enumerates no specs and is not all_invariants`);
    }
  }
  return { surfaces, ids, problems };
}

function resolveOwners(world, surfaces, surfaceIds) {
  const owners = new Map();
  const problems = [];
  for (const surface of surfaces) {
    for (const id of Array.isArray(surface.specs) ? surface.specs : []) {
      if (typeof id !== 'string') {
        problems.push(`manifest: surface ${surface.id} has a non-string spec id`);
        continue;
      }
      if (!world.specs.has(id)) {
        problems.push(`manifest: surface ${surface.id} names missing spec ${id}`);
      }
    }
    for (const prefix of Array.isArray(surface.spec_prefixes) ? surface.spec_prefixes : []) {
      if (typeof prefix !== 'string' || prefix.length === 0) {
        problems.push(`manifest: surface ${surface.id} has an empty spec prefix`);
      }
    }
  }
  for (const [id, spec] of world.specs) {
    const explicit = surfaces.filter((surface) => (surface.specs ?? []).includes(id));
    if (explicit.length > 1) {
      problems.push(`unmapped-spec: ${id} is claimed by several surfaces (${explicit.map((s) => s.id).join(', ')})`);
      continue;
    }
    if (explicit.length === 1) {
      owners.set(id, explicit[0].id);
      continue;
    }
    if (typeof spec.surface === 'string' && surfaceIds.has(spec.surface)) {
      owners.set(id, spec.surface);
      continue;
    }
    const prefixed = surfaces.filter((surface) =>
      (surface.spec_prefixes ?? []).some((prefix) => id.startsWith(prefix)),
    );
    if (prefixed.length === 1) {
      owners.set(id, prefixed[0].id);
    } else if (prefixed.length === 0) {
      problems.push(`unmapped-spec: ${id} belongs to no surface (add it to tests/mutation-surfaces.json)`);
    } else {
      problems.push(
        `unmapped-spec: ${id} matches several surface prefixes (${prefixed.map((s) => s.id).join(', ')})`,
      );
    }
  }
  return { owners, problems };
}

export function surfaceProblems(world) {
  const problems = [];
  if (world.manifest?.schema !== SCHEMA) {
    problems.push(`manifest: schema must be ${SCHEMA}`);
  }
  const { surfaces, ids: surfaceIds, problems: surfaceProblems } = surfacesOf(world);
  problems.push(...surfaceProblems);

  for (const spec of world.specs.values()) {
    problems.push(...checkSpec(spec, surfaceIds));
  }

  const { owners, problems: ownerProblems } = resolveOwners(world, surfaces, surfaceIds);
  problems.push(...ownerProblems);

  for (const surface of surfaces) {
    if (typeof surface.id !== 'string') continue;
    if (surface.all_invariants === true) {
      if (world.invariants.size === 0) {
        problems.push(`surface ${surface.id}: all_invariants is declared but the invariant registry is empty`);
      }
      continue;
    }
    const count = [...owners.values()].filter((owner) => owner === surface.id).length;
    if (count === 0) {
      problems.push(`uncovered-surface: ${surface.id} has no planted mutation spec`);
    }
  }

  // Every invariant is a surface-critical behaviour and must name its own
  // executable mutation command bound to an existing spec.
  for (const [id, invariant] of world.invariants) {
    const spec = world.specs.get(id);
    if (spec === undefined) {
      problems.push(`invariant-without-mutation: ${id} has no spec scripts/mutations/${id}.json`);
    } else {
      if (owners.get(id) === undefined) {
        problems.push(`invariant-without-surface: ${id} resolves to no mutation surface`);
      }
      if (invariant.mutationCommand !== `node scripts/mutation-run.mjs ${id}`) {
        problems.push(
          `invariant-mutation-command: ${id} must run "node scripts/mutation-run.mjs ${id}" (got ${JSON.stringify(invariant.mutationCommand)})`,
        );
      }
    }
  }
  for (const id of world.specs.keys()) {
    if (!world.invariants.has(id)) {
      problems.push(`spec-without-invariant: ${id} has no registry entry in tests/invariants.toml`);
    }
  }

  // Programme rows: every kind present in the registry must be claimed by a
  // surface, so a new kind (a new surface family) cannot ship without a
  // mutation gate owner. Rows naming an invariant must name a proven one.
  const kindOwners = new Map();
  for (const surface of surfaces) {
    for (const kind of Array.isArray(surface.row_kinds) ? surface.row_kinds : []) {
      if (kindOwners.has(kind)) {
        problems.push(`manifest: row kind ${kind} is claimed by both ${kindOwners.get(kind)} and ${surface.id}`);
        continue;
      }
      kindOwners.set(kind, surface.id);
    }
  }
  const presentKinds = new Set();
  for (const row of world.rows) {
    if (typeof row?.kind !== 'string') {
      problems.push(`programme-row: ${row?.id ?? '<row>'} has no kind`);
      continue;
    }
    presentKinds.add(row.kind);
    const owner = kindOwners.get(row.kind);
    if (owner === undefined) {
      problems.push(`unmapped-row-kind: programme row ${row.id ?? '<row>'} has kind ${row.kind}, which no surface claims`);
    } else {
      const surface = surfaces.find((candidate) => candidate.id === owner);
      const prefixes = surface?.row_prefixes?.[row.kind];
      if (Array.isArray(prefixes) && prefixes.length > 0) {
        const name = String(row.id ?? '');
        if (!prefixes.some((prefix) => name.startsWith(prefix))) {
          problems.push(
            `unmapped-programme-row: ${name} (kind ${row.kind}) matches no ${owner} row prefix`,
          );
        }
      }
    }
    for (const testId of Array.isArray(row.test_ids) ? row.test_ids : []) {
      if (typeof testId !== 'string' || !SPEC_ID.test(testId)) continue;
      if (!world.invariants.has(testId)) {
        problems.push(`programme-row: ${row.id ?? '<row>'} names unregistered invariant ${testId}`);
      } else if (!world.specs.has(testId)) {
        problems.push(`programme-row: ${row.id ?? '<row>'} names invariant ${testId} without a mutation`);
      }
    }
  }
  for (const kind of presentKinds) {
    if (!kindOwners.has(kind)) {
      problems.push(`unmapped-row-kind: registry carries kind ${kind}, which no surface claims`);
    }
  }
  return { problems, owners, surfaceIds, presentKinds };
}

export function campaignProblems(world, campaignPath, owners = null) {
  const problems = [];
  if (!existsSync(campaignPath)) {
    return [`campaign: evidence file ${campaignPath} does not exist`];
  }
  let campaign;
  try {
    campaign = readJson(campaignPath);
  } catch (error) {
    return [`campaign: ${campaignPath} is not valid JSON: ${error.message}`];
  }
  const scope = campaign.scope === 'per-surface' ? 'per-surface' : 'full';
  const results = Array.isArray(campaign.results) ? campaign.results : [];
  const byId = new Map();
  for (const result of results) {
    if (typeof result?.id !== 'string') {
      problems.push('campaign: a result has no spec id');
      continue;
    }
    if (byId.has(result.id)) problems.push(`campaign: duplicate result for ${result.id}`);
    byId.set(result.id, result);
  }
  if (scope === 'full') {
    for (const id of world.specs.keys()) {
      if (!byId.has(id)) problems.push(`campaign-incomplete: ${id} has no evidence result`);
    }
  } else {
    // A bounded per-surface campaign proves one detected mutation per covered
    // surface. It never weakens the survivor rule: every result that WAS run
    // must be a detection, and every declared surface must have one.
    const surfaces = Array.isArray(world.manifest.surfaces) ? world.manifest.surfaces : [];
    const surfaceIds = new Set(surfaces.map((surface) => surface.id).filter((id) => typeof id === 'string'));
    const surfaceOwners = owners ?? resolveOwners(world, surfaces, surfaceIds).owners;
    const covered = new Set();
    for (const id of byId.keys()) {
      const owner = surfaceOwners.get(id);
      if (owner !== undefined) covered.add(owner);
    }
    for (const surface of world.manifest.surfaces ?? []) {
      if (typeof surface.id !== 'string') continue;
      if (surface.all_invariants === true) continue;
      if (!covered.has(surface.id)) {
        problems.push(`campaign-surface-uncovered: ${surface.id} has no detected mutation result`);
      }
    }
  }
  for (const [id, result] of byId) {
    if (!world.specs.has(id)) {
      problems.push(`campaign: evidence names unknown spec ${id}`);
      continue;
    }
    if (result.outcome !== 'detected') {
      problems.push(`survivor: ${id} outcome is ${JSON.stringify(result.outcome)} (${result.reason ?? 'no reason'})`);
    }
    if (result.exit_code !== 0) {
      problems.push(`survivor: ${id} runner exit ${result.exit_code}, not a detection`);
    }
  }
  for (const [countKey, label] of [
    ['survivors', 'survivor'],
    ['environment_failures', 'environment'],
    ['not_run', 'not_run'],
  ]) {
    const value = campaign[countKey];
    if (Number.isInteger(value) && value > 0) {
      problems.push(`campaign: ${value} ${label} result(s) must fail the campaign`);
    }
  }
  return problems;
}

function stats(world, presentKinds) {
  const kinds = {};
  for (const row of world.rows) {
    kinds[row.kind] = (kinds[row.kind] ?? 0) + 1;
  }
  return {
    surfaces: (world.manifest.surfaces ?? []).length,
    specs: world.specs.size,
    invariants: world.invariants.size,
    rows: world.rows.length,
    kinds: Object.fromEntries(Object.entries(kinds).sort(([a], [b]) => a.localeCompare(b))),
    presentKinds: [...presentKinds].sort(),
  };
}

function run(args) {
  let world;
  try {
    world = loadWorld(resolve(argValue(args, '--root', ROOT)));
  } catch (error) {
    console.error(`check-mutation-surfaces: ${error.message}`);
    return 1;
  }
  const checked = surfaceProblems(world);
  const problems = [...checked.problems];
  const campaignPath = argValue(args, '--campaign', '');
  if (campaignPath !== '') {
    problems.push(...campaignProblems(world, resolve(campaignPath), checked.owners));
  }
  if (problems.length > 0) {
    for (const problem of problems) console.error(`mutation-surfaces: ${problem}`);
    console.error(`check-mutation-surfaces: FAIL (${problems.length} violation(s))`);
    return 1;
  }
  const summary = stats(world, checked.presentKinds);
  const kinds = Object.entries(summary.kinds)
    .map(([kind, count]) => `${kind}=${count}`)
    .join(', ');
  console.log(
    `check-mutation-surfaces: PASS (${summary.surfaces} surfaces, ${summary.specs} specs, ` +
      `${summary.invariants} invariants, ${summary.rows} programme rows: ${kinds})`,
  );
  return 0;
}

function selftest() {
  let failures = 0;
  const check = (name, ok) => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}`);
      failures += 1;
    }
  };
  const spec = (id, overrides = {}) => ({
    id,
    gate: `cargo test -p faktor-alpha --lib ${id.toLowerCase().replaceAll('-', '_')}`,
    expect: 'FAILED',
    __file: `${id}.json`,
    ...overrides,
  });
  const invariant = (id) => ({
    id,
    mutationCommand: `node scripts/mutation-run.mjs ${id}`,
    releaseCritical: true,
  });
  const world = (overrides = {}) => ({
    manifest: {
      schema: SCHEMA,
      surfaces: [{ id: 'core', row_kinds: ['control'], specs: ['INV-A'] }],
    },
    specs: new Map([['INV-A', spec('INV-A')]]),
    invariants: new Map([['INV-A', invariant('INV-A')]]),
    rows: [{ id: 'ui.control.a', kind: 'control', test_ids: ['INV-A'] }],
    ...overrides,
  });
  check('the clean synthetic world passes', surfaceProblems(world()).problems.length === 0);
  check(
    'a spec without an expected failure signature fails',
    surfaceProblems(
      world({ specs: new Map([['INV-A', spec('INV-A', { expect: undefined })]]) }),
    ).problems.some((problem) => problem.includes('missing expected failure signature')),
  );
  check(
    'a new invariant without a mutation fails',
    surfaceProblems(
      world({ invariants: new Map([...world().invariants, ['INV-B', invariant('INV-B')]]) }),
    ).problems.some((problem) => problem.includes('invariant-without-mutation: INV-B')),
  );
  check(
    'a new spec in an unknown family fails',
    surfaceProblems(
      world({ specs: new Map([...world().specs, ['INV-GHOST', spec('INV-GHOST')]]) }),
    ).problems.some((problem) => problem.includes('unmapped-spec: INV-GHOST')),
  );
  check(
    'a new programme row kind fails',
    surfaceProblems(
      world({ rows: [{ id: 'new.surface', kind: 'new-kind', test_ids: [] }] }),
    ).problems.some((problem) => problem.includes('unmapped-row-kind')),
  );
  const campaign = {
    schema: 'faktor-mutation-campaign/v1',
    results: [{ id: 'INV-A', outcome: 'undetected', exit_code: 1, reason: 'gate stayed green' }],
    survivors: 1,
  };
  const tmp = join(ROOT, 'target', 'mutation-surfaces-selftest.json');
  mkdirSync(dirname(tmp), { recursive: true });
  writeFileSync(tmp, JSON.stringify(campaign));
  check(
    'a survivor fails the campaign',
    campaignProblems(world(), tmp).some((problem) => problem.includes('survivor: INV-A')),
  );
  writeFileSync(tmp, JSON.stringify({ schema: 'faktor-mutation-campaign/v1', results: [] }));
  check(
    'a missing campaign result fails',
    campaignProblems(world(), tmp).some((problem) => problem.includes('campaign-incomplete: INV-A')),
  );
  rmSync(tmp, { force: true });
  if (failures > 0) {
    console.error(`check-mutation-surfaces selftest: FAIL (${failures})`);
    return 1;
  }
  console.log('check-mutation-surfaces selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('check-mutation-surfaces.mjs');
if (isMain) {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) process.exit(selftest());
  if (args.includes('--check') || args.includes('--campaign') || args.length === 0) {
    process.exit(run(args));
  }
  console.error('usage: check-mutation-surfaces.mjs [--check] [--campaign FILE] [--root DIR] | --selftest');
  process.exit(2);
}
