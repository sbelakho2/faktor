#!/usr/bin/env node
// Environment-specific visual certification checker (audit 28).
//
// The JetBrains visual parity baseline (schema
// `faktor-parity-visual-baselines/v3`) must carry a DISTINCT record per
// release platform (`linux`, `macos`, `windows`). A platform with no record
// is "not certified on platform <p>" — it is never inherited from another
// platform's digests (the v2 canonical `panelDigests` pool is refused).
//
// Modes:
//   --check PATH    (default path = apps/jetbrains/frontend/src/test/resources/
//                    parity/visual-baselines.json)
//                   Validate the schema and print the coverage table. Exit 1
//                   on a malformed/v2 baseline, an unknown platform key, or
//                   an incomplete record; a MISSING required platform is
//                   reported (not-certified) and exits 0.
//   --release PATH  Release claim gate: exit 1 when any required platform has
//                   no certified record, printing "not certified on platform <p>".
//   --selftest      Adversarial fixtures: missing platform, v2 file, unknown
//                   platform, malformed digest, and complete coverage.
//
// Exit codes: 0 pass (or expected failure matched), 1 violation, 2 usage.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULT_BASELINE =
  'apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json';
const SCHEMA = 'faktor-parity-visual-baselines/v3';
const REQUIRED_PLATFORMS = ['linux', 'macos', 'windows'];
const DIGEST = /^[0-9a-f]{64}$/;

function fail(message) {
  console.error(`check-visual-platforms: ${message}`);
  process.exit(1);
}

/** Parses and validates the baseline; returns { coverage, problems }. */
export function evaluateBaseline(text, requiredPlatforms = REQUIRED_PLATFORMS) {
  const problems = [];
  let root;
  try {
    root = JSON.parse(text);
  } catch (e) {
    return { coverage: {}, problems: [`baseline is not valid JSON: ${e.message}`] };
  }
  if (root.schema !== SCHEMA) {
    problems.push(
      `baseline schema is ${JSON.stringify(root.schema)}; expected ${SCHEMA} ` +
        '(v2 canonical panelDigests are never inherited as platform records)',
    );
    return { coverage: {}, problems };
  }
  const declared = Array.isArray(root.requiredPlatforms) ? root.requiredPlatforms : [];
  if (JSON.stringify(declared) !== JSON.stringify(requiredPlatforms)) {
    problems.push(
      `requiredPlatforms is ${JSON.stringify(declared)}; expected ` +
        JSON.stringify(requiredPlatforms),
    );
  }
  const platforms = root.platforms && typeof root.platforms === 'object' ? root.platforms : {};
  for (const key of Object.keys(platforms)) {
    if (!requiredPlatforms.includes(key)) {
      problems.push(`unknown platform key ${JSON.stringify(key)}`);
    }
  }
  const coverage = {};
  for (const platform of requiredPlatforms) {
    const record = platforms[platform];
    if (record === undefined) {
      coverage[platform] = 'not_certified';
      continue;
    }
    if (typeof record !== 'object' || record === null) {
      problems.push(`platform ${platform} record is not an object`);
      coverage[platform] = 'not_certified';
      continue;
    }
    if (typeof record.environment !== 'string' || record.environment.length === 0) {
      problems.push(`platform ${platform} has no environment fingerprint`);
    }
    const digests = record.digests;
    if (typeof digests !== 'object' || digests === null || Object.keys(digests).length === 0) {
      problems.push(`platform ${platform} has no digest records`);
      coverage[platform] = 'not_certified';
      continue;
    }
    let ok = true;
    for (const [panel, digest] of Object.entries(digests)) {
      if (typeof digest !== 'string' || !DIGEST.test(digest)) {
        problems.push(`platform ${platform} panel ${panel} is not a 64-hex digest`);
        ok = false;
      }
    }
    coverage[platform] = ok && problems.length === 0 ? 'certified' : 'not_certified';
  }
  return { coverage, problems };
}

function report(coverage) {
  for (const platform of REQUIRED_PLATFORMS) {
    const state = coverage[platform] || 'not_certified';
    const label = state === 'certified' ? 'certified' : `not certified on platform ${platform}`;
    console.log(`visual baseline: ${platform}: ${label}`);
  }
}

function run(path, release) {
  if (!existsSync(path)) {
    fail(
      release
        ? `release claim refused: no visual baseline at ${path}`
        : `visual baseline not found at ${path}`,
    );
  }
  const { coverage, problems } = evaluateBaseline(readFileSync(path, 'utf8'));
  for (const problem of problems) {
    console.error(`check-visual-platforms: ${problem}`);
  }
  report(coverage);
  if (problems.length > 0) {
    process.exit(1);
  }
  if (release) {
    const missing = REQUIRED_PLATFORMS.filter((p) => coverage[p] !== 'certified');
    if (missing.length > 0) {
      console.error(
        `check-visual-platforms: release claim refused — not certified on platform ` +
          `${missing.join(', ')}`,
      );
      process.exit(1);
    }
    console.log('check-visual-platforms: release claim covered on every required platform');
  }
  process.exit(0);
}

function fixture(overrides = {}) {
  const record = {
    environment: 'linux-amd64-jvm17',
    digests: { 'task-tree': 'a'.repeat(64), settings: 'b'.repeat(64) },
  };
  const base = {
    schema: SCHEMA,
    requiredPlatforms: [...REQUIRED_PLATFORMS],
    platforms: {
      linux: JSON.parse(JSON.stringify(record)),
      macos: {
        environment: 'mac-os-x-aarch64-jvm17',
        digests: { 'task-tree': 'c'.repeat(64), settings: 'd'.repeat(64) },
      },
      windows: {
        environment: 'windows-amd64-jvm17',
        digests: { 'task-tree': 'e'.repeat(64), settings: 'f'.repeat(64) },
      },
    },
  };
  return JSON.stringify({ ...base, ...overrides });
}

function selftest() {
  let failed = 0;
  const expectProblems = (label, text, wantProblems) => {
    const { problems } = evaluateBaseline(text);
    const ok = wantProblems ? problems.length > 0 : problems.length === 0;
    if (!ok) {
      console.error(`check-visual-platforms selftest: FAIL ${label}: ${problems.join('; ')}`);
      failed += 1;
    }
  };
  expectProblems('complete fixture', fixture(), false);

  const noWindows = JSON.parse(fixture());
  delete noWindows.platforms.windows;
  const missing = evaluateBaseline(JSON.stringify(noWindows));
  if (missing.problems.length > 0) {
    console.error('check-visual-platforms selftest: FAIL missing windows reported malformed');
    failed += 1;
  }
  if (missing.coverage.windows !== 'not_certified') {
    console.error('check-visual-platforms selftest: FAIL missing windows must be not_certified');
    failed += 1;
  }
  if (missing.coverage.linux !== 'certified' || missing.coverage.macos !== 'certified') {
    console.error('check-visual-platforms selftest: FAIL present platforms must stay certified');
    failed += 1;
  }
  // The v2 canonical pool can never certify a platform.
  expectProblems(
    'v2 file',
    JSON.stringify({ schema: 'faktor-parity-visual-baselines/v2', panelDigests: {} }),
    true,
  );
  const v2 = evaluateBaseline(
    JSON.stringify({ schema: 'faktor-parity-visual-baselines/v2', panelDigests: {} }),
  );
  if (Object.values(v2.coverage).some((state) => state === 'certified')) {
    console.error('check-visual-platforms selftest: FAIL v2 must certify nothing');
    failed += 1;
  }
  const unknown = JSON.parse(fixture());
  unknown.platforms.solaris = { environment: 'x', digests: { 'task-tree': 'a'.repeat(64) } };
  expectProblems('unknown platform', JSON.stringify(unknown), true);
  const badDigest = JSON.parse(fixture());
  badDigest.platforms.linux.digests.settings = 'not-a-digest';
  expectProblems('malformed digest', JSON.stringify(badDigest), true);
  const emptyDigests = JSON.parse(fixture());
  emptyDigests.platforms.macos.digests = {};
  expectProblems('empty digest record', JSON.stringify(emptyDigests), true);
  if (failed > 0) process.exit(1);
  console.log('check-visual-platforms selftest: PASS');
}

const args = process.argv.slice(2);
if (args[0] === '--selftest') {
  selftest();
} else if (args[0] === '--check' || args[0] === '--release') {
  const path = resolve(ROOT, args[1] || DEFAULT_BASELINE);
  run(path, args[0] === '--release');
} else {
  console.error(
    'usage: node scripts/check-visual-platforms.mjs [--check|--release] [PATH] | --selftest',
  );
  process.exit(2);
}
