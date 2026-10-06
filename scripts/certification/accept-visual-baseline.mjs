#!/usr/bin/env node
// Accept one platform's freshly produced JetBrains visual record into the
// committed baseline (P1-CERT/UI operator path).
//
// The real platform record must come from a render ON that platform:
//   Windows:  powershell -File scripts/windows-visual-baseline.ps1 -WriteBaselines
//   macOS:    bash apps/jetbrains/compile-and-smoke.sh --write-baselines
// Both write the merged baseline file to target/certification/. This tool
// validates the produced record (fingerprint present, non-empty digests,
// distinct from every other platform) and merges ONLY that platform's entry
// into apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json.
//
// Usage:
//   node scripts/certification/accept-visual-baseline.mjs <platform> <produced.json> [--baseline PATH]
//   node scripts/certification/accept-visual-baseline.mjs selftest

import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const DEFAULT_BASELINE = resolve(
  ROOT,
  'apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json',
);

export function mergePlatform(baseline, produced, platform) {
  const record = produced?.platforms?.[platform];
  if (record === undefined) {
    throw new Error(`the produced document has no platforms.${platform} record`);
  }
  const merged = JSON.parse(JSON.stringify(baseline));
  merged.platforms = merged.platforms ?? {};
  merged.platforms[platform] = record;
  return merged;
}

function validate(record, platform, baseline) {
  const problems = [];
  const environment = record?.environment;
  if (typeof environment !== 'string' || !/-f[0-9a-f]{8,}$/.test(environment)) {
    problems.push(`${platform}: environment lacks a resolved -f<hex> font fingerprint`);
  }
  const digests = record?.digests;
  if (record === undefined || typeof digests !== 'object' || Object.keys(digests).length === 0) {
    problems.push(`${platform}: no panel digests`);
  } else {
    for (const [panel, digest] of Object.entries(digests)) {
      if (typeof digest !== 'string' || digest.length === 0) {
        problems.push(`${platform}/${panel}: empty digest`);
      }
    }
    for (const [other, otherRecord] of Object.entries(baseline.platforms ?? {})) {
      if (other !== platform && JSON.stringify(otherRecord?.digests) === JSON.stringify(digests)) {
        problems.push(`${platform}: digests identical to ${other}'s (a copied record is not proof)`);
      }
    }
  }
  return problems;
}

function main(args) {
  const positional = args.filter((arg) => !arg.startsWith('--'));
  const platform = positional[0];
  const producedPath = positional[1];
  const baselineAt = args.indexOf('--baseline');
  const baselinePath = baselineAt === -1 ? DEFAULT_BASELINE : resolve(args[baselineAt + 1]);
  if (!platform || !producedPath) {
    console.error(
      'usage: accept-visual-baseline.mjs <platform> <produced.json> [--baseline PATH]',
    );
    return 2;
  }
  const baseline = JSON.parse(readFileSync(baselinePath, 'utf8'));
  let produced;
  try {
    produced = JSON.parse(readFileSync(producedPath, 'utf8'));
  } catch (error) {
    console.error(`accept-visual-baseline: cannot read ${producedPath}: ${error.message}`);
    return 1;
  }
  const record = produced?.platforms?.[platform];
  if (record === undefined) {
    console.error(`accept-visual-baseline: ${producedPath} has no platforms.${platform} record`);
    return 1;
  }
  const problems = validate(record, platform, baseline);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`accept-visual-baseline: ${problem}`);
    return 1;
  }
  const merged = mergePlatform(baseline, produced, platform);
  writeFileSync(baselinePath, `${JSON.stringify(merged, null, 2)}\n`);
  // End-to-end: the canonical per-platform checker must now accept it.
  try {
    execFileSync(
      'python3',
      [
        resolve(ROOT, 'scripts/certification/check-visual-baseline.py'),
        '--platform',
        platform,
        '--file',
        baselinePath,
      ],
      { stdio: 'inherit' },
    );
  } catch {
    console.error('accept-visual-baseline: the canonical checker still refuses the merged baseline');
    return 1;
  }
  console.log(`accept-visual-baseline: merged the ${platform} record into ${baselinePath}`);
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
  const baseline = {
    platforms: { linux: { environment: 'linux-f0a1b2c3d4e5', digests: { a: 'a'.repeat(64) } } },
  };
  const good = {
    platforms: { windows: { environment: 'windows-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } } },
  };
  check('a valid record merges', mergePlatform(baseline, good, 'windows').platforms.windows !== undefined);
  check('a fingerprint-less record refuses', validate({ environment: 'win', digests: { a: 'c' } }, 'windows', baseline).length > 0);
  check(
    'a copied record refuses',
    validate({ environment: 'windows-f0a1b2c3d4e5', digests: { a: 'a'.repeat(64) } }, 'windows', baseline).some((p) =>
      p.includes('identical'),
    ),
  );
  check(
    'a missing record refuses',
    (() => {
      try {
        mergePlatform(baseline, { platforms: {} }, 'windows');
        return false;
      } catch {
        return true;
      }
    })(),
  );
  if (failures > 0) {
    console.error(`accept-visual-baseline selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('accept-visual-baseline selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('accept-visual-baseline.mjs');
if (isMain) {
  if (process.argv.includes('selftest')) {
    process.exit(selftest());
  }
  process.exit(main(process.argv.slice(2)));
}
