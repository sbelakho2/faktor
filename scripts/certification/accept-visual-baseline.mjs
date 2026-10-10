#!/usr/bin/env node
// Accept one platform's freshly produced JetBrains visual record into the
// committed baseline (P1-CERT/UI operator path).
//
// The record must come from a render ON the platform it claims, and this
// tool refuses to run anywhere else. `validate()` binds the target platform
// to (a) the host OS executing the tool, (b) the environment prefix the
// Kotlin `visualEnvironment()` writer stamps per host (`linux-`, `mac-os-`/
// `darwin-`, `windows-`), and (c) the resolved `-f<hex>` font fingerprint.
// A linux run can therefore never write the macos/windows record, and a
// copied or fingerprint-less record is refused.
//
// One documented command per platform (renders, validates and pins on THIS
// host):
//   linux:   node scripts/certification/accept-visual-baseline.mjs linux --render
//   macos:   node scripts/certification/accept-visual-baseline.mjs macos --render
//   windows: node scripts/certification/accept-visual-baseline.mjs windows --render
//            (Git Bash; the owned certifying lane is instead
//             powershell -NoProfile -ExecutionPolicy Bypass -File
//             scripts/windows-visual-baseline.ps1 -WriteBaselines)
// `--render` runs `bash apps/jetbrains/compile-and-smoke.sh --write-baselines`
// on this host; the Kotlin writer detects the host platform itself (it is
// never passed in) and re-pins only that platform's record in the in-tree
// baseline with its resolved-font environment fingerprint.
//
// Accepting a produced record file instead (e.g. the Windows lane's captured
// target/certification/visual-baselines-windows.json):
//   node scripts/certification/accept-visual-baseline.mjs <platform> <produced.json> [--baseline PATH]
//
// Selftest (no render, hermetic fixtures):
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

/** The release platform this Node host IS; anything else is `unknown`. */
const HOST_PLATFORM_BY_NODE = { linux: 'linux', darwin: 'macos', win32: 'windows' };

export function hostPlatform(platform = process.platform) {
  return HOST_PLATFORM_BY_NODE[platform] ?? 'unknown';
}

/** The environment prefixes the Kotlin `visualEnvironment()` writer stamps
 * per host. `darwin-` is macOS's own alternate spelling, never another
 * platform's. */
export const PLATFORM_ENVIRONMENT_PREFIXES = {
  linux: ['linux-'],
  macos: ['mac-os-', 'darwin-'],
  windows: ['windows-'],
};

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

/**
 * Validates one produced platform record. The host binding comes FIRST: a
 * record for another platform is refused before any content check, so a
 * linux run can never merge a macos/windows record (and vice versa).
 */
export function validate(record, platform, baseline, host = hostPlatform()) {
  const problems = [];
  const prefixes = PLATFORM_ENVIRONMENT_PREFIXES[platform];
  if (prefixes === undefined) {
    problems.push(
      `${platform}: unknown platform (expected linux, macos or windows)`,
    );
    return problems;
  }
  if (platform !== host) {
    problems.push(
      `${platform}: this host is ${host}; a ${platform} record is only accepted ` +
        `on a ${platform} host (a cross-platform pin is refused)`,
    );
    return problems;
  }
  const environment = record?.environment;
  if (typeof environment !== 'string' || !/-f[0-9a-f]{8,}$/.test(environment)) {
    problems.push(`${platform}: environment lacks a resolved -f<hex> font fingerprint`);
  } else if (!prefixes.some((prefix) => environment.startsWith(prefix))) {
    problems.push(
      `${platform}: environment '${environment}' does not name the ${platform} host ` +
        `(prefixes ${prefixes.join(', ')}); another platform's render is never accepted`,
    );
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

function parseArgs(args) {
  const positional = [];
  let baselinePath = null;
  let render = false;
  for (let at = 0; at < args.length; at += 1) {
    const arg = args[at];
    if (arg === '--render') {
      render = true;
      continue;
    }
    if (arg === '--baseline') {
      if (args[at + 1] === undefined) {
        throw new Error('--baseline requires a path');
      }
      baselinePath = resolve(args[at + 1]);
      at += 1;
      continue;
    }
    positional.push(arg);
  }
  return {
    positional,
    baselinePath: baselinePath ?? DEFAULT_BASELINE,
    render,
  };
}

function main(args) {
  let parsed;
  try {
    parsed = parseArgs(args);
  } catch (error) {
    console.error(`accept-visual-baseline: ${error.message}`);
    return 2;
  }
  const { positional, render } = parsed;
  const platform = positional[0];
  let producedPath = positional[1] === undefined ? null : resolve(positional[1]);
  const baselinePath = parsed.baselinePath;
  if (!platform) {
    console.error(
      'usage: accept-visual-baseline.mjs <platform> [produced.json] [--render] [--baseline PATH]',
    );
    return 2;
  }
  const host = hostPlatform();
  if (PLATFORM_ENVIRONMENT_PREFIXES[platform] === undefined || platform !== host) {
    console.error(
      `accept-visual-baseline: refusing to accept a ${platform} record on this ${host} host; ` +
        'render and pin only on the platform\'s own host',
    );
    return 1;
  }
  if (render) {
    if (producedPath) {
      console.error('accept-visual-baseline: --render takes no produced file (the render pins in-tree)');
      return 2;
    }
    try {
      execFileSync('bash', ['apps/jetbrains/compile-and-smoke.sh', '--write-baselines'], {
        cwd: ROOT,
        stdio: 'inherit',
      });
    } catch (error) {
      console.error(`accept-visual-baseline: the ${platform} render failed: ${error.message}`);
      return 1;
    }
    // The Kotlin writer pinned THIS host's record in-tree; validate that.
    producedPath = baselinePath;
  }
  if (!producedPath) {
    console.error(
      'usage: accept-visual-baseline.mjs <platform> <produced.json> [--baseline PATH]; ' +
        'or <platform> --render',
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
  const problems = validate(record, platform, baseline, host);
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
  check(
    'a fingerprint-less record refuses',
    validate({ environment: 'win', digests: { a: 'c' } }, 'windows', baseline, 'windows').length > 0,
  );
  check(
    'a copied record refuses',
    validate(
      { environment: 'windows-f0a1b2c3d4e5', digests: { a: 'a'.repeat(64) } },
      'windows',
      baseline,
      'windows',
    ).some((p) => p.includes('identical')),
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
  // Host binding: the core "a linux run can never write another platform's
  // record" guarantee, tested both ways.
  check(
    'a linux host refuses a macos record',
    validate(
      { environment: 'mac-os-x-aarch64-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } },
      'macos',
      baseline,
      'linux',
    ).some((p) => p.includes('this host is linux')),
  );
  check(
    'a macos host refuses a windows record',
    validate(
      { environment: 'windows-amd64-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } },
      'windows',
      baseline,
      'macos',
    ).some((p) => p.includes('this host is macos')),
  );
  // Environment provenance: a fingerprinted linux render under the macos key
  // (same host) is still foreign.
  check(
    'a foreign environment refuses on the claimed host',
    validate(
      { environment: 'linux-amd64-jvm17-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } },
      'macos',
      baseline,
      'macos',
    ).some((p) => p.includes('does not name the macos host')),
  );
  check(
    'the darwin alias is accepted as macos provenance',
    validate(
      { environment: 'darwin-arm64-jvm17-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } },
      'macos',
      baseline,
      'macos',
    ).length === 0,
  );
  check(
    'an unknown platform refuses',
    validate({ environment: 'x-f0a1b2c3d4e5', digests: { a: 'b'.repeat(64) } }, 'freebsd', baseline, 'linux').some(
      (p) => p.includes('unknown platform'),
    ),
  );
  // Option parsing: `--baseline PATH` must never leak PATH into positional.
  check(
    'option values never become positionals',
    (() => {
      const parsed = parseArgs(['macos', '--render', '--baseline', '/tmp/b.json']);
      return parsed.render && parsed.baselinePath === '/tmp/b.json' && parsed.positional.length === 1;
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
