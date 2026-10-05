#!/usr/bin/env node
// Mechanical capability-coverage check (audit P1-ASSURANCE).
//
// The invariant registry proves that every REGISTERED invariant has a
// mutation-killed witness; it does not prove that every production
// capability is registered. This checker closes that hole mechanically: it
// enumerates every local non-test package from `cargo metadata`, binds each
// to an explicit disposition in tests/invariant-coverage.json, and refuses:
//
//   * an unclassified new production crate,
//   * a `release-critical` disposition whose owner invariant does not exist,
//     is not release-critical, or whose registry text never names the crate,
//   * a `covered` disposition without existing release-critical via owners,
//   * a `gap` without a documented reason (visible debt, not silence),
//   * a disposition for a crate that no longer exists.
//
// Modes:
//   node scripts/check-invariant-coverage.mjs --check    (passes with gaps;
//                                                        prints the debt count)
//   node scripts/check-invariant-coverage.mjs --release  (gaps are failures)
//   node scripts/check-invariant-coverage.mjs selftest
//
// Exit codes: 0 pass; 1 violations (or gaps under --release); 2 usage/parse.

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const COVERAGE_FILE = join(ROOT, 'tests/invariant-coverage.json');
const REGISTRY_FILE = join(ROOT, 'tests/invariants.toml');

/** Every local package that is a production capability (not a test harness). */
export function productionCrates(metadata) {
  return metadata.packages
    .filter((pkg) => pkg.source === null)
    .map((pkg) => pkg.name)
    .filter((name) => !name.startsWith('faktor-tests-'))
    .sort();
}

/** id -> {releaseCritical, owner, authority} from the TOML registry. */
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

function mentionsCrate(invariant, crate) {
  const short = crate.replace(/^faktor-/, '');
  const haystack = `${invariant.owner} ${invariant.authority}`;
  return [crate, `crates/${short}`, `apps/${short}`].some((needle) => haystack.includes(needle));
}

export function coverageProblems({ crates, invariants, coverage }) {
  const problems = [];
  const entries = Array.isArray(coverage.entries) ? coverage.entries : [];
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
      if (owners.length === 0) {
        problems.push(`missing-owner: ${crate} is release-critical with no owner invariant`);
      }
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
    } else if (entry.class === 'covered') {
      const via = Array.isArray(entry.via) ? entry.via : [];
      if (via.length === 0) {
        problems.push(`missing-via: ${crate} is covered but names no via invariant`);
      }
      if (typeof entry.reason !== 'string' || entry.reason.trim().length < 10) {
        problems.push(`missing-reason: ${crate} must document why it is covered`);
      }
      for (const owner of via) {
        const invariant = invariants.get(owner);
        if (invariant === undefined) {
          problems.push(`unknown-via: ${crate} names ${owner}, which is not registered`);
        } else if (!invariant.releaseCritical) {
          problems.push(`non-critical-via: ${crate} names ${owner}, which is not release_critical`);
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
  return problems;
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
  };
}

function gapCount(coverage) {
  return (coverage.entries || []).filter((entry) => entry.class === 'gap').length;
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
  if (mode === '--release' && gaps > 0) {
    console.error(
      `check-invariant-coverage: FAIL (release mode: ${gaps} production capability gap(s) have no mutation-killed invariant)`,
    );
    return 1;
  }
  console.log(
    `check-invariant-coverage: PASS (${world.crates.length} production crates classified, ${gaps} documented gap(s))`,
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
    ['INV-B', { releaseCritical: false, owner: 'crates/beta', authority: 'crates/beta' }],
  ]);
  const world = (entries) => ({
    crates: ['faktor-alpha', 'faktor-beta'],
    invariants,
    coverage: { schema: 'faktor-invariant-coverage/v1', entries },
  });
  const good = [
    { crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-A'] },
    { crate: 'faktor-beta', class: 'gap', reason: 'documented debt for the beta surface' },
  ];
  check('a complete classification passes', coverageProblems(world(good)).length === 0);
  check(
    'an unclassified crate fails',
    coverageProblems(
      world([good[0], { crate: 'faktor-betamax', class: 'gap', reason: 'x'.repeat(30) }]),
    ).some((problem) => problem.includes('unclassified-crate: faktor-beta')),
  );
  check(
    'an unknown owner fails',
    coverageProblems(
      world([{ crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-NOPE'] }, good[1]]),
    ).some((problem) => problem.includes('unknown-owner')),
  );
  check(
    'an owner that never names the crate fails',
    coverageProblems(
      world([{ crate: 'faktor-alpha', class: 'release-critical', owners: ['INV-B'] }, good[1]]),
    ).some((problem) => problem.includes('non-critical-owner') || problem.includes('unbound-owner')),
  );
  check(
    'an undocumented gap fails',
    coverageProblems(world([good[0], { crate: 'faktor-beta', class: 'gap', reason: 'x' }])).some(
      (problem) => problem.includes('undocumented-gap'),
    ),
  );
  check(
    'a stale entry fails',
    coverageProblems(
      world([good[0], good[1], { crate: 'faktor-ghost', class: 'gap', reason: 'x'.repeat(30) }]),
    ).some((problem) => problem.includes('stale-entry')),
  );
  check(
    'a covered entry without via fails',
    coverageProblems(
      world([
        good[0],
        { crate: 'faktor-beta', class: 'covered', reason: 'covered transitively somewhere' },
      ]),
    ).some((problem) => problem.includes('missing-via')),
  );
  // The real manifest + registry must be internally consistent.
  const real = load();
  check(
    'the real coverage manifest is consistent with cargo metadata and the registry',
    coverageProblems(real).length === 0,
    );
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
  if (args.includes('selftest')) {
    process.exit(selftest());
  }
  if (args.includes('--check')) {
    process.exit(run('--check'));
  }
  if (args.includes('--release')) {
    process.exit(run('--release'));
  }
  console.error('usage: check-invariant-coverage.mjs --check | --release | selftest');
  process.exit(2);
}
