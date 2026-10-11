#!/usr/bin/env node
// Top-level prove-all gate (audit P1-PROOF / release closure).
//
// The certificate chain proves individual authorities. This gate binds the
// OFFLINE-provable subset to the ACTUAL CHECKOUT BYTES being tested — not
// merely HEAD^{tree}: it hashes every tracked file's worktree content (so an
// uncommitted edit changes the digest), refuses a dirty checkout outright
// (certifying bytes must be the committed bytes), runs the registry,
// coverage, workflow-graph, publisher, soak and visual selftests, and emits
// target/certification/prove-all.json.
//
// Classes (`--class`, default `ci-evidence`):
//   * ci-evidence: a MISSING platform visual record is recorded as a typed,
//     visible `not_certified-on-platform` boundary (never a passed check,
//     never silently green); the report also carries the release claim's
//     own status (`release_claim.status = refused` until every required
//     record is committed). Everything else is a hard failure.
//   * release: the same checks run with the release semantics — a missing
//     platform refuses (the release claim gate stays intact).
//
// Facts that can only be observed at pipeline runtime are listed explicitly
// under `not_proven_here` (platform certificate authentication, the mutation
// campaign's tree barrier, quiescent convergence) — they are enforced by
// their own lanes, never silently assumed here.
//
// Usage:
//   node scripts/certification/prove-all.mjs [--class ci-evidence|release]
//   node scripts/certification/prove-all.mjs selftest
//
// Exit codes: 0 proven; 1 any offline check failed, the checkout is dirty,
// or (release class) a required platform is not certified.

import { execFileSync, spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { lstatSync, mkdirSync, readFileSync, readlinkSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = join(ROOT, 'target/certification/prove-all.json');
const VISUAL_BASELINE = 'apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json';
const CI_EVIDENCE = 'ci-evidence';
const RELEASE = 'release';
const CLASSES = [CI_EVIDENCE, RELEASE];
const PLATFORM_BOUNDARY_EXIT = 3;
const VISUAL_PLATFORMS = ['linux', 'macos', 'windows'];

/**
 * Content hash of every tracked worktree file (committed or not), including
 * the executable/symlink METADATA that decides what the checked-out tree can
 * actually run: mode bits and symlink targets are part of the proof input,
 * not merely file contents.
 */
export function checkoutDigest(root) {
  const files = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8' })
    .split('\0')
    .filter((file) => file.length > 0)
    .sort();
  const hash = createHash('sha256');
  for (const file of files) {
    hash.update(`${file}\0`);
    let info;
    try {
      info = lstatSync(join(root, file));
    } catch {
      // A tracked file deleted from the worktree is part of the checked-out
      // state: hash the deletion marker, never skip it.
      hash.update('<deleted>\0');
      continue;
    }
    hash.update(`mode=${info.mode.toString(8)}\0`);
    if (info.isSymbolicLink()) {
      hash.update(`symlink=${readlinkSync(join(root, file))}\0`);
    } else if (info.isFile()) {
      hash.update(readFileSync(join(root, file)));
      hash.update('\0');
    } else {
      hash.update(`kind=${info.isDirectory() ? 'dir' : 'other'}\0`);
    }
  }
  return { digest: hash.digest('hex'), files: files.length };
}

export function dirtyCheckout(root) {
  try {
    execFileSync('git', ['diff', '--exit-code', '--quiet'], { cwd: root, stdio: 'pipe' });
    execFileSync('git', ['diff', '--cached', '--exit-code', '--quiet'], { cwd: root, stdio: 'pipe' });
    return null;
  } catch {
    return 'tracked working-tree modifications or staged changes';
  }
}

const COMMON_CHECKS = [
  { name: 'invariants', command: ['node', ['scripts/check-invariants.mjs']] },
  { name: 'coverage', command: ['node', ['scripts/check-invariant-coverage.mjs', '--release']] },
  { name: 'discovery', command: ['node', ['scripts/discover-capabilities.mjs', 'selftest']] },
  { name: 'workflow-graph', command: ['node', ['scripts/certification/check-workflow-graph.mjs', '--check']] },
  { name: 'publisher', command: ['node', ['scripts/certification/publish-status.mjs', 'selftest']] },
  { name: 'soak-sampler', command: ['python3', ['scripts/certification/soak-convergence.py', 'selftest']] },
  { name: 'visual-record-selftest', command: ['python3', ['scripts/certification/check-visual-baseline.py', 'selftest']] },
  // Every REQUIRED platform record, including its resolved font fingerprint:
  // the real committed baseline, never a synthetic fixture. In the
  // CI-evidence class a MISSING record is a typed platform boundary (exit 3,
  // recorded, never green); a present-but-broken record is a hard failure in
  // every class. The release class refuses the whole run on a missing record.
  ...VISUAL_PLATFORMS.map((platform) => ({
    name: `visual-${platform}`,
    platform,
    command: [
      'python3',
      ['scripts/certification/check-visual-baseline.py', '--platform', platform,
       '--file', VISUAL_BASELINE],
    ],
  })),
  { name: 'contracts-emit', command: ['node', ['scripts/contracts/emit.mjs', 'selftest']] },
  // P2: the REAL checked-in contract digest, not merely the emitter's
  // selftest — a stale generated contract must fail inside prove-all itself.
  { name: 'contracts-verify', command: ['node', ['scripts/contracts/emit.mjs', 'verify', '--print-digest']] },
  { name: 'compiled-proofs-gate', command: ['python3', ['scripts/certification/check-capability-tests-compiled.py', 'selftest']] },
  { name: 'visual-accept', command: ['node', ['scripts/certification/accept-visual-baseline.mjs', 'selftest']] },
];

/** The class-aware check list: the release class runs the platform-record
 * checks with the release semantics (`--class release`), and additionally
 * runs the release claim gate (`check-visual-platforms.mjs --release`) as a
 * hard check. The CI-evidence class records coverage (`--check`, which
 * reports missing platforms without failing) and treats a missing record as
 * a typed platform boundary. */
export function checksForClass(cls) {
  const checks = COMMON_CHECKS.map((check) =>
    check.platform
      ? { ...check, command: [check.command[0], [...check.command[1], '--class', cls]] }
      : check,
  );
  const coverage = {
    name: 'visual-record-coverage',
    command: ['node', ['scripts/check-visual-platforms.mjs', cls === RELEASE ? '--release' : '--check']],
  };
  // Keep the coverage gate next to the platform checks (after them, so the
  // per-platform refusal detail is already in the report).
  const at = checks.findIndex((check) => check.name === 'contracts-emit');
  checks.splice(at, 0, coverage);
  return checks;
}

/** Map one check exit status to a report entry. Only the CI-evidence class
 * may downgrade a MISSING platform (the checker's typed exit 3) to a
 * visible `not_certified-on-platform` boundary; everything else, in every
 * class, is a failure. */
export function checkOutcome(check, cls, exitCode) {
  if (exitCode === 0) {
    return { name: check.name, status: 'passed' };
  }
  if (cls === CI_EVIDENCE && check.platform && exitCode === PLATFORM_BOUNDARY_EXIT) {
    return {
      name: check.name,
      status: 'not_certified-on-platform',
      platform: check.platform,
    };
  }
  return { name: check.name, status: 'failed' };
}

function runChecks(cls) {
  const results = [];
  let failed = false;
  for (const check of checksForClass(cls)) {
    const [command, args] = check.command;
    try {
      execFileSync(command, args, {
        cwd: ROOT,
        stdio: 'pipe',
        timeout: 10 * 60 * 1000,
        maxBuffer: 64 * 1024 * 1024,
      });
      results.push(checkOutcome(check, cls, 0));
    } catch (error) {
      const tail = `${error.stdout ?? ''}${error.stderr ?? ''}`.slice(-800);
      const outcome = checkOutcome(check, cls, error.status ?? 1);
      results.push({ ...outcome, tail });
      if (outcome.status === 'failed') {
        failed = true;
      }
    }
  }
  // CI-evidence: record the release claim's own status as visible evidence
  // (never green). The release class enforces it through the coverage check.
  let releaseClaim = null;
  if (cls === CI_EVIDENCE) {
    const probe = spawnSync('node', ['scripts/check-visual-platforms.mjs', '--release'], {
      cwd: ROOT,
      encoding: 'utf8',
      timeout: 60 * 1000,
    });
    releaseClaim = {
      status: probe.status === 0 ? 'covered' : 'refused',
      detail: `${probe.stdout ?? ''}${probe.stderr ?? ''}`.trim().slice(-800),
      checked_at: 'prove-all',
    };
  }
  return { results, failed, releaseClaim };
}

function main(argv) {
  let cls = CI_EVIDENCE;
  for (let at = 0; at < argv.length; at += 1) {
    if (argv[at] === '--class' && at + 1 < argv.length) {
      cls = argv[at + 1];
      at += 1;
    } else {
      console.error(`prove-all: unknown argument ${argv[at]}`);
      return 2;
    }
  }
  if (!CLASSES.includes(cls)) {
    console.error(`prove-all: --class must be one of ${CLASSES.join(', ')} (got ${cls})`);
    return 2;
  }
  // PROOF SANDWICH (P1-PROOF): hash the actual checkout bytes BEFORE and
  // AFTER every sub-check and refuse unless the tree stayed byte- and
  // metadata-identical the whole time. A sub-gate that mutates source (or
  // flips an executable bit) can no longer be certified from stale state.
  const dirtyBefore = dirtyCheckout(ROOT);
  if (dirtyBefore !== null) {
    console.error(`prove-all: FAIL — the checkout is dirty (${dirtyBefore}); certifying bytes must be committed`);
    return 1;
  }
  const before = checkoutDigest(ROOT);
  const { results, failed, releaseClaim } = runChecks(cls);
  let contractDigest = null;
  try {
    contractDigest = execFileSync('node', ['scripts/contracts/emit.mjs', 'verify', '--print-digest'], {
      cwd: ROOT,
      encoding: 'utf8',
    }).trim();
  } catch {
    contractDigest = null;
  }
  const dirtyAfter = dirtyCheckout(ROOT);
  const after = checkoutDigest(ROOT);
  const platformBoundaries = results
    .filter((entry) => entry.status === 'not_certified-on-platform')
    .map((entry) => ({
      check: entry.name,
      platform: entry.platform,
      status: 'not_certified-on-platform',
    }));
  const report = {
    schema: 'faktor-prove-all/v3',
    class: cls,
    checkout: {
      digest_before: `sha256:${before.digest}`,
      digest_after: `sha256:${after.digest}`,
      stable: before.digest === after.digest && dirtyAfter === null,
      files: before.files,
      clean: dirtyAfter === null,
    },
    checks: results,
    platform_boundaries: platformBoundaries,
    release_claim: releaseClaim,
    contract_digest: contractDigest,
    not_proven_here: [
      'platform certificate authentication (ci/faktor/trusted-certified-{linux,darwin,windows} signatures) — publish-status aggregate lane',
      'mutation campaign tree barrier and isolated-copy execution — check-invariants --mutations lane',
      'quiescent convergence (RSS/FD/children/queues/WAL/temp/CAS/latency) — soak lanes',
      'real VS Code Extension Host and JetBrains packaged-ZIP host matrices — vscode-e2e / jetbrains-smoke lanes',
    ],
  };
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${JSON.stringify(report, null, 2)}\n`);
  for (const boundary of platformBoundaries) {
    console.error(
      `prove-all: platform boundary — ${boundary.platform}: not_certified-on-platform ` +
        `(${boundary.check}; visible, never green; the release claim stays refused)`,
    );
  }
  if (releaseClaim && releaseClaim.status !== 'covered') {
    console.error(
      `prove-all: release claim refused (recorded, not a platform certification): ${releaseClaim.detail}`,
    );
  }
  if (dirtyAfter !== null) {
    console.error(`prove-all: FAIL — the checkout became dirty during the proof (${dirtyAfter})`);
    return 1;
  }
  if (before.digest !== after.digest) {
    console.error(
      `prove-all: FAIL — the checkout bytes changed during the proof (${before.digest} -> ${after.digest})`,
    );
    return 1;
  }
  if (failed) {
    for (const result of results.filter((entry) => entry.status === 'failed')) {
      console.error(`prove-all: check ${result.name} FAILED\n${result.tail}`);
    }
    console.error(`prove-all: FAIL (checkout sha256:${before.digest}, ${before.files} files)`);
    return 1;
  }
  const boundaryNote =
    platformBoundaries.length > 0
      ? `; ${platformBoundaries.length} typed platform boundary ` +
        `(${platformBoundaries.map((entry) => `${entry.platform}=not_certified-on-platform`).join(', ')})`
      : '';
  console.log(
    `prove-all: PASS [${cls}] (checkout sha256:${before.digest} stable before and after, ` +
      `${before.files} files, ${results.length} offline checks${boundaryNote})`,
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
  const dir = execFileSync('mktemp', ['-d'], { encoding: 'utf8' }).trim();
  try {
    execFileSync('git', ['init', '--quiet'], { cwd: dir });
    writeFileSync(join(dir, 'a.txt'), 'one');
    execFileSync('git', ['add', 'a.txt'], { cwd: dir });
    execFileSync(
      'git',
      ['-c', 'user.email=selftest@example.invalid', '-c', 'user.name=selftest', 'commit', '--quiet', '-m', 'x'],
      { cwd: dir },
    );
    const clean = checkoutDigest(dir);
    check('a clean checkout has no dirty report', dirtyCheckout(dir) === null);
    // An UNCOMMITTED edit must change the checkout digest even though
    // HEAD^{tree} is unchanged — the whole point of this gate.
    writeFileSync(join(dir, 'a.txt'), 'two');
    const edited = checkoutDigest(dir);
    check('an uncommitted edit changes the checkout digest', edited.digest !== clean.digest);
    check('an uncommitted edit is reported dirty', dirtyCheckout(dir) !== null);
    // A deleted tracked file is part of the checked-out state.
    rmSync(join(dir, 'a.txt'));
    check('a deleted tracked file changes the digest', checkoutDigest(dir).digest !== edited.digest);
    // Executable/symlink metadata is part of the proof input.
    writeFileSync(join(dir, 'a.txt'), 'two');
    execFileSync('git', ['add', 'a.txt'], { cwd: dir });
    execFileSync(
      'git',
      ['-c', 'user.email=selftest@example.invalid', '-c', 'user.name=selftest', 'commit', '--quiet', '-m', 'y'],
      { cwd: dir },
    );
    const plain = checkoutDigest(dir).digest;
    execFileSync('chmod', ['+x', join(dir, 'a.txt')]);
    check('a mode-bit change changes the checkout digest', checkoutDigest(dir).digest !== plain);
    rmSync(join(dir, 'a.txt'));
    execFileSync('ln', ['-s', 'target.txt', join(dir, 'link.txt')]);
    execFileSync('git', ['add', 'a.txt', 'link.txt'], { cwd: dir });
    const withLink = checkoutDigest(dir).digest;
    rmSync(join(dir, 'link.txt'));
    execFileSync('ln', ['-s', 'other.txt', join(dir, 'link.txt')]);
    check('a symlink target change changes the checkout digest', checkoutDigest(dir).digest !== withLink);

    // CI-evidence vs release class at the checker boundary: a MISSING
    // platform record is a typed `not_certified-on-platform` boundary (exit
    // 3) only for CI evidence; the release class refuses (exit 1); both
    // classes certify a complete record, and a malformed record is NEVER a
    // boundary.
    const baseline = join(dir, 'visual-baselines.json');
    const record = (environment, seed) => ({
      environment,
      digests: { 'task-tree': seed.repeat(64) },
    });
    const partial = {
      schema: 'faktor-parity-visual-baselines/v3',
      requiredPlatforms: ['linux', 'macos', 'windows'],
      platforms: {
        linux: record('linux-amd64-jvm17-f29cb65792a65', 'a'),
        macos: record('mac-os-x-aarch64-jvm17-f9f93b9e02d24', 'b'),
      },
    };
    writeFileSync(baseline, JSON.stringify(partial));
    const runWindows = (cls) =>
      spawnSync(
        'python3',
        ['scripts/certification/check-visual-baseline.py', '--class', cls,
         '--platform', 'windows', '--file', baseline],
        { cwd: ROOT, encoding: 'utf8' },
      );
    check('ci-evidence: missing windows is the typed boundary (exit 3)', runWindows(CI_EVIDENCE).status === 3);
    check('release: missing windows still refuses (exit 1)', runWindows(RELEASE).status === 1);
    const complete = JSON.parse(JSON.stringify(partial));
    complete.platforms.windows = record('windows-amd64-jvm17-fcafef00d123', 'c');
    writeFileSync(baseline, JSON.stringify(complete));
    check('ci-evidence: complete coverage passes', runWindows(CI_EVIDENCE).status === 0);
    check('release: complete coverage passes', runWindows(RELEASE).status === 0);
    const broken = JSON.parse(JSON.stringify(complete));
    broken.platforms.windows.environment = 'windows-amd64-jvm17';
    writeFileSync(baseline, JSON.stringify(broken));
    check(
      'ci-evidence: a present-but-broken record is never a boundary',
      runWindows(CI_EVIDENCE).status === 1,
    );
    check(
      'checkOutcome maps exit 3 to a boundary only in the ci-evidence class',
      checkOutcome({ name: 'visual-windows', platform: 'windows' }, CI_EVIDENCE, 3).status ===
        'not_certified-on-platform' &&
        checkOutcome({ name: 'visual-windows', platform: 'windows' }, RELEASE, 3).status ===
          'failed' &&
        checkOutcome({ name: 'visual-windows', platform: 'windows' }, CI_EVIDENCE, 1).status ===
          'failed',
    );
    check(
      'checksForClass passes the class to platform checks and gates coverage by class',
      checksForClass(CI_EVIDENCE).some(
        (entry) =>
          entry.name === 'visual-windows' && entry.command[1].includes(CI_EVIDENCE),
      ) &&
        checksForClass(RELEASE).some(
          (entry) =>
            entry.name === 'visual-record-coverage' && entry.command[1].includes('--release'),
        ),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  if (failures > 0) {
    console.error(`prove-all selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('prove-all selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('prove-all.mjs');
if (isMain) {
  if (process.argv.includes('selftest')) {
    process.exit(selftest());
  }
  process.exit(main(process.argv.slice(2)));
}
