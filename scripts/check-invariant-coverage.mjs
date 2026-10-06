#!/usr/bin/env node
// Mechanical capability-coverage check (audits P0-2, P0-3, P1-4).
//
// Three soundness rules, all mechanical:
//
//  1. DISCOVERY EQUALITY (P0-2): the real exposed surfaces are discovered
//     from the compiled/source registries (native routes, builtin tools,
//     CLI command tree, provider kinds, VS Code contributions, JetBrains
//     plugin contributions) by scripts/discover-capabilities.mjs. Every
//     discovered id must exist in the proof manifest (as a capability row,
//     surface row, or a `discovered` row) — adding a route/tool/command
//     without a proof row turns the checker red in BOTH modes.
//
//  2. DIRECT CAPABILITY MUTATIONS (P0-3): a `proven` capability must be
//     bound to a planted mutation whose gate actually runs this capability's
//     oracle test (scripts/mutations/*.json `capabilities` + `gate`). A
//     shared mutation that never runs the capability's test is an
//     `unbound-mutation` violation, not a proof.
//
//  3. NO WHOLE-CRATE EXEMPTIONS (P1-4): release mode rejects `covered`
//     crates; per-capability `delegated` rows (delegate -> another proven
//     capability or a registered release-critical invariant) replace them.
//     Release mode also fails on any capability `gap`.
//
// Modes:
//   --check    offline/source gate: discovery equality, unit existence,
//              binding, delegation validity; gaps are reported as debt.
//   --release  additionally fails on gaps AND on whole-crate `covered`.
//   selftest   synthetic violation corpus + the real manifest must pass.

import { execFileSync } from 'node:child_process';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { discover } from './discover-capabilities.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const COVERAGE_FILE = join(ROOT, 'tests/invariant-coverage.json');
const REGISTRY_FILE = join(ROOT, 'tests/invariants.toml');
const MUTATIONS_DIR = join(ROOT, 'scripts/mutations');
const SURFACES = ['apps/vscode', 'apps/jetbrains'];

export function productionCrates(metadata) {
  return metadata.packages
    .filter((pkg) => pkg.source === null)
    .map((pkg) => pkg.name)
    .filter((name) => !name.startsWith('faktor-tests-'))
    .sort();
}

export function parseInvariants(text) {
  const invariants = new Map();
  for (const block of text.split('[[invariant]]').slice(1)) {
    const id = /^id\s*=\s*"(.*)"$/m.exec(block);
    if (!id) continue;
    const owner = /^owner\s*=\s*"(.*)"$/m.exec(block);
    const authority = /^authority\s*=\s*"(.*)"$/m.exec(block);
    const critical = /^release_critical\s*=\s*(true|false)/m.exec(block);
    invariants.set(id[1], {
      releaseCritical: critical ? critical[1] === 'true' : false,
      owner: owner ? owner[1] : '',
      authority: authority ? authority[1] : '',
    });
  }
  return invariants;
}

/** id -> { capabilities, gate } for every planted mutation spec. */
export function loadMutationSpecs(dir = MUTATIONS_DIR) {
  const specs = new Map();
  for (const name of readdirSync(dir).sort()) {
    if (!name.endsWith('.json')) continue;
    try {
      const document = JSON.parse(readFileSync(join(dir, name), 'utf8'));
      if (typeof document.id === 'string' && typeof document.gate === 'string') {
        specs.set(document.id, {
          capabilities: Array.isArray(document.capabilities) ? document.capabilities : [],
          gate: document.gate,
        });
      }
    } catch {
      // A malformed spec is surfaced by `mutation-run --check-specs`; the
      // coverage check simply does not count it as a binding.
    }
  }
  return specs;
}

function walkFiles(root, filter, out = []) {
  let entries;
  try {
    const info = statSync(root);
    if (info.isFile()) {
      if (filter(root)) out.push(root);
      return out;
    }
    entries = readdirSync(root);
  } catch {
    return out;
  }
  for (const name of entries) {
    const path = join(root, name);
    let info;
    try {
      info = statSync(path);
    } catch {
      continue;
    }
    if (info.isDirectory()) {
      if (name === 'node_modules' || name === 'target' || name === 'build') continue;
      walkFiles(path, filter, out);
    } else if (filter(path)) {
      out.push(path);
    }
  }
  return out;
}

export function rustFnCount(dir, name) {
  let count = 0;
  for (const file of walkFiles(dir, (path) => path.endsWith('.rs'))) {
    const text = readFileSync(file, 'utf8');
    const matches = text.match(new RegExp(`fn\\s+${name.replace(/[$]/g, '\\$')}\\s*\\(`, 'g'));
    if (matches) count += matches.length;
  }
  return count;
}

export function titleCount(dir, title) {
  let count = 0;
  const files = walkFiles(dir, (path) => /\.(mjs|js|ts|tsx|kt|kts)$/.test(path));
  for (const file of files) {
    const text = readFileSync(file, 'utf8');
    let at = 0;
    for (;;) {
      const found = text.indexOf(title, at);
      if (found === -1) break;
      count += 1;
      at = found + title.length;
    }
  }
  return count;
}

const CAPABILITY_ID = /^[a-z0-9-]+(\.[a-z0-9_-]+)+$/;

export function coverageProblems({
  crates,
  invariants,
  coverage,
  discovered = { ids: [] },
  specs = new Map(),
  root = ROOT,
}) {
  const problems = [];
  const entries = Array.isArray(coverage.entries) ? coverage.entries : [];
  const surfaces = Array.isArray(coverage.surfaces) ? coverage.surfaces : [];
  const discoveredRows = coverage.discovered && typeof coverage.discovered === 'object'
    ? coverage.discovered
    : {};
  const byCrate = new Map();
  for (const entry of entries) {
    if (typeof entry.crate !== 'string' || entry.crate.length === 0) {
      problems.push('invalid-entry: an entry has no crate name');
      continue;
    }
    if (byCrate.has(entry.crate)) {
      problems.push(`duplicate-entry: ${entry.crate}`);
      continue;
    }
    byCrate.set(entry.crate, entry);
  }

  // ---- global capability registry --------------------------------------
  const capabilities = new Map(); // id -> row (with surface/entry context)
  const collect = (label, isSurface, rows) => {
    for (const row of rows) {
      if (typeof row.id !== 'string' || !CAPABILITY_ID.test(row.id)) {
        problems.push(`bad-capability-id: ${label} has ${JSON.stringify(row.id)}`);
        continue;
      }
      if (capabilities.has(row.id)) {
        problems.push(`duplicate-capability: ${row.id}`);
        continue;
      }
      capabilities.set(row.id, { ...row, label, isSurface });
    }
  };
  for (const [crate, entry] of byCrate) {
    if (!crates.includes(crate)) continue;
    if (entry.class === 'release-critical') {
      collect(crate, false, Array.isArray(entry.capabilities) ? entry.capabilities : []);
    }
  }
  for (const surface of surfaces) {
    collect(surface.id ?? 'surface', true, Array.isArray(surface.capabilities) ? surface.capabilities : []);
  }
  for (const [id, row] of Object.entries(discoveredRows)) {
    collect('discovered', false, [{ ...row, id }]);
  }

  const effectiveClass = (row) =>
    row.class === 'gap' ? 'gap' : typeof row.delegate === 'string' && row.delegate.length > 0 ? 'delegated' : 'proven';

  const validateRow = (row) => {
    const cls = effectiveClass(row);
    if (cls === 'gap') {
      if (typeof row.reason !== 'string' || row.reason.trim().length < 20) {
        problems.push(`undocumented-gap: ${row.id} needs a concrete reason (>= 20 chars)`);
      }
      return;
    }
    if (cls === 'delegated') {
      const target = row.delegate;
      if (invariants.has(target)) {
        if (!invariants.get(target).releaseCritical) {
          problems.push(`non-critical-delegate: ${row.id} delegates to non-critical invariant ${target}`);
        }
      } else if (capabilities.has(target)) {
        // Cycle detection by walking the delegation chain.
        const seen = new Set([row.id]);
        let cursor = capabilities.get(target);
        while (cursor !== undefined && effectiveClass(cursor) === 'delegated') {
          if (seen.has(cursor.id)) {
            problems.push(`delegation-cycle: ${row.id} -> ... -> ${cursor.id}`);
            cursor = undefined;
            break;
          }
          seen.add(cursor.id);
          cursor = capabilities.get(cursor.delegate);
        }
        if (cursor === undefined) {
          problems.push(`unknown-delegate: ${row.id} delegates to ${target}, which resolves nowhere`);
        } else if (effectiveClass(cursor) !== 'proven') {
          problems.push(`unproven-delegate: ${row.id} delegates to ${cursor.id}, which is not proven`);
        }
      } else {
        problems.push(`unknown-delegate: ${row.id} delegates to ${JSON.stringify(target)}, which is neither a capability nor a registered invariant`);
      }
      if (typeof row.reason !== 'string' || row.reason.trim().length < 10) {
        problems.push(`missing-delegation-reason: ${row.id}`);
      }
      return;
    }
    // proven
    const unit = typeof row.unit === 'string' ? row.unit.trim() : '';
    if (unit === '') {
      problems.push(`missing-unit: ${row.id} has no unit proof`);
    } else {
      const sourceRoot = row.source
        ? join(root, row.source)
        : row.isSurface
          ? join(root, row.surface ?? row.label)
          : join(root, 'crates', row.label.replace(/^faktor-/, ''));
      const count = row.isSurface ? titleCount(sourceRoot, unit) : rustFnCount(sourceRoot, unit);
      if (count !== 1) {
        problems.push(
          `unproven-unit: ${row.id} proof ${JSON.stringify(unit)} appears ${count} time(s) under ${relative(root, sourceRoot) || '.'} (need exactly 1)`,
        );
      }
    }
    // P0-3 binding: some planted mutation must run THIS capability's gate.
    let bound = false;
    for (const [, spec] of specs) {
      if (spec.capabilities.includes(row.id) && spec.gate.includes(unit) && unit !== '') {
        bound = true;
        break;
      }
    }
    if (!bound) {
      problems.push(
        `unbound-mutation: ${row.id} is marked proven but no planted mutation gate runs its oracle ${JSON.stringify(unit)} (add a direct mutation or reclassify as gap/delegated)`,
      );
    }
  };

  for (const row of capabilities.values()) {
    if (row.id === undefined) continue;
    validateRow(row);
  }

  // ---- discovery equality (P0-2) ---------------------------------------
  const known = new Set(capabilities.keys());
  for (const id of discovered.ids ?? []) {
    if (!known.has(id)) {
      problems.push(
        `undiscovered-capability: ${id} exists on a real product surface but has no manifest row (prove it, delegate it, or record an explicit gap)`,
      );
    }
  }

  // ---- crate classification --------------------------------------------
  for (const crate of crates) {
    if (!byCrate.has(crate)) {
      problems.push(`unclassified-crate: ${crate} (add an explicit disposition)`);
    }
  }
  for (const [crate, entry] of byCrate) {
    if (!crates.includes(crate)) {
      problems.push(`stale-entry: ${crate} is not a local production package`);
      continue;
    }
    if (entry.class === 'release-critical') {
      const owners = Array.isArray(entry.owners) ? entry.owners : [];
      for (const owner of owners) {
        const invariant = invariants.get(owner);
        if (invariant === undefined) {
          problems.push(`unknown-owner: ${crate} names ${owner}, which is not registered`);
        } else if (!invariant.releaseCritical) {
          problems.push(`non-critical-owner: ${crate} is owned by ${owner}, which is not release_critical`);
        } else if (!mentionsCrate(invariant, crate)) {
          problems.push(
            `unbound-owner: ${crate} names ${owner}, whose registry text never names the crate (bind it or reclassify)`,
          );
        }
      }
      if ((Array.isArray(entry.capabilities) ? entry.capabilities : []).length === 0) {
        problems.push(`missing-capabilities: ${crate} is release-critical but enumerates no capability`);
      }
    } else if (entry.class === 'covered') {
      // Whole-crate transitive coverage is legal in --check as visible debt,
      // but release mode refuses it (P1-4): per-capability delegation only.
      if (typeof entry.reason !== 'string' || entry.reason.trim().length < 10) {
        problems.push(`missing-reason: ${crate} must document why it is covered`);
      }
      for (const owner of Array.isArray(entry.via) ? entry.via : []) {
        if (!invariants.has(owner)) {
          problems.push(`unknown-via: ${crate} names ${owner}, which is not registered`);
        }
      }
    } else if (entry.class === 'gap') {
      if (typeof entry.reason !== 'string' || entry.reason.trim().length < 20) {
        problems.push(`undocumented-gap: ${crate} needs a concrete reason (>= 20 chars)`);
      }
    } else {
      problems.push(`unknown-class: ${crate} has class ${JSON.stringify(entry.class)}`);
    }
  }
  for (const required of SURFACES) {
    if (!surfaces.some((surface) => surface.id === required)) {
      problems.push(`missing-surface: ${required} has no capability rows`);
    }
  }
  return problems;
}

function mentionsCrate(invariant, crate) {
  const short = crate.replace(/^faktor-/, '');
  const haystack = `${invariant.owner} ${invariant.authority}`;
  return [crate, `crates/${short}`, `apps/${short}`].some((needle) => haystack.includes(needle));
}

function load() {
  const metadata = JSON.parse(
    execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
      cwd: ROOT,
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    }),
  );
  return {
    crates: productionCrates(metadata),
    invariants: parseInvariants(readFileSync(REGISTRY_FILE, 'utf8')),
    coverage: JSON.parse(readFileSync(COVERAGE_FILE, 'utf8')),
    discovered: discover(ROOT),
    specs: loadMutationSpecs(),
  };
}

function gapCount(coverage) {
  let gaps = 0;
  for (const entry of coverage.entries ?? []) {
    for (const row of entry.capabilities ?? []) {
      if (row.class === 'gap') gaps += 1;
    }
  }
  for (const surface of coverage.surfaces ?? []) {
    for (const row of surface.capabilities ?? []) {
      if (row.class === 'gap') gaps += 1;
    }
  }
  for (const row of Object.values(coverage.discovered ?? {})) {
    if (row.class === 'gap') gaps += 1;
  }
  return gaps;
}

function run(mode) {
  let world;
  try {
    world = load();
  } catch (error) {
    console.error(`check-invariant-coverage: ${error.message}`);
    return 1;
  }
  const problems = coverageProblems(world);
  const gaps = gapCount(world.coverage);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`check-invariant-coverage: ${problem}`);
    console.error(`check-invariant-coverage: FAIL (${problems.length} violation(s))`);
    return 1;
  }
  const coveredCrates =
    mode === '--release'
      ? (world.coverage.entries ?? []).filter((entry) => entry.class === 'covered')
      : [];
  if (coveredCrates.length > 0) {
    for (const entry of coveredCrates) {
      console.error(
        `check-invariant-coverage: covered-crate-in-release: ${entry.crate} must enumerate per-capability delegated rows instead of a whole-crate exemption`,
      );
    }
    console.error(`check-invariant-coverage: FAIL (release mode)`);
    return 1;
  }
  if (mode === '--release' && gaps > 0) {
    console.error(
      `check-invariant-coverage: FAIL (release mode: ${gaps} capability gap(s) have no direct mutation-killed proof)`,
    );
    return 1;
  }
  const rows = (world.coverage.entries ?? []).reduce(
    (sum, entry) => sum + (Array.isArray(entry.capabilities) ? entry.capabilities.length : 0),
    0,
  );
  console.log(
    `check-invariant-coverage: PASS (${world.crates.length} crates, ${rows} crate capability rows, ${Object.keys(world.coverage.discovered ?? {}).length} discovered-surface rows, ${world.discovered.ids.length} discovered ids, ${gaps} documented gap(s))`,
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
  const invariants = new Map([
    ['INV-A', { releaseCritical: true, owner: 'crates/alpha', authority: 'crates/alpha/src/lib.rs' }],
    ['INV-B', { releaseCritical: false, owner: 'crates/alpha', authority: 'crates/alpha' }],
  ]);
  const specs = new Map([
    ['MUT-ALPHA', { capabilities: ['alpha.authority'], gate: 'cargo test -p faktor-alpha --lib production_entry_is_bounded' }],
  ]);
  const world = (entries, discoveredIds = ['alpha.authority'], discoveredRows = {}) => ({
    crates: ['faktor-alpha', 'faktor-beta'],
    invariants,
    specs,
    discovered: { ids: discoveredIds },
    coverage: {
      schema: 'faktor-invariant-coverage/v3',
      entries,
      surfaces: [
        { id: 'apps/vscode', capabilities: [] },
        { id: 'apps/jetbrains', capabilities: [] },
      ],
      discovered: discoveredRows,
    },
    root: ROOT,
  });
  const good = [
    { crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-A'], capabilities: [{ id: 'alpha.authority', unit: 'no_such_fn_xyz', mutation: 'INV-A' }] },
    { crate: 'faktor-beta', class: 'gap', reason: 'documented debt for the beta surface' },
  ];
  check(
    'an unproven synthetic unit is reported (never silently trusted)',
    coverageProblems(world(good)).some((problem) => problem.includes('unproven-unit')),
  );
  check(
    'an unbound mutation is reported',
    coverageProblems(world([{ crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-A'], capabilities: [] }, good[1]])).some(
      (problem) => problem.includes('missing-capabilities'),
    ),
  );
  check(
    'an undiscovered new product capability fails in check mode',
    coverageProblems(world(good, ['alpha.authority', 'native.get.ghost'])).some((problem) =>
      problem.includes('undiscovered-capability: native.get.ghost'),
    ),
  );
  check(
    'a delegated row pointing nowhere fails',
    coverageProblems(
      world([
        { crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-A'], capabilities: [{ id: 'alpha.other', class: 'delegated', delegate: 'nowhere.at.all', reason: 'x'.repeat(12) }] },
        good[1],
      ], [], { 'alpha.discovered': { class: 'delegated', delegate: 'nowhere.at.all', reason: 'x'.repeat(12) } }),
    ).some((problem) => problem.includes('unknown-delegate')),
  );
  check(
    'an undocumented gap fails',
    coverageProblems(world([good[0], { crate: 'faktor-beta', class: 'gap', reason: 'x' }])).some((problem) =>
      problem.includes('undocumented-gap'),
    ),
  );
  // The real manifest + registry + discovery + specs must be consistent.
  const real = load();
  const realProblems = coverageProblems(real);
  check(`the real capability graph is internally consistent (${realProblems.length} problem(s))`, realProblems.length === 0);
  for (const problem of realProblems.slice(0, 8)) console.error(`  ${problem}`);
  if (failures > 0) {
    console.error(`check-invariant-coverage selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('check-invariant-coverage selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('check-invariant-coverage.mjs');
if (isMain) {
  const args = process.argv.slice(2);
  if (args.includes('selftest')) process.exit(selftest());
  if (args.includes('--check')) process.exit(run('--check'));
  if (args.includes('--release')) process.exit(run('--release'));
  console.error('usage: check-invariant-coverage.mjs --check | --release | selftest');
  process.exit(2);
}
