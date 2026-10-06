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
// Facts that can only be observed at pipeline runtime are listed explicitly
// under `not_proven_here` (platform certificate authentication, the mutation
// campaign's tree barrier, quiescent convergence) — they are enforced by
// their own lanes, never silently assumed here.
//
// Usage:
//   node scripts/certification/prove-all.mjs
//   node scripts/certification/prove-all.mjs selftest
//
// Exit codes: 0 proven; 1 any offline check failed or the checkout is dirty.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { lstatSync, mkdirSync, readFileSync, readlinkSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = join(ROOT, 'target/certification/prove-all.json');

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

const CHECKS = [
  { name: 'invariants', command: ['node', ['scripts/check-invariants.mjs']] },
  { name: 'coverage', command: ['node', ['scripts/check-invariant-coverage.mjs', '--release']] },
  { name: 'discovery', command: ['node', ['scripts/discover-capabilities.mjs', 'selftest']] },
  { name: 'workflow-graph', command: ['node', ['scripts/certification/check-workflow-graph.mjs', '--check']] },
  { name: 'publisher', command: ['node', ['scripts/certification/publish-status.mjs', 'selftest']] },
  { name: 'soak-sampler', command: ['python3', ['scripts/certification/soak-convergence.py', 'selftest']] },
  { name: 'visual-record-selftest', command: ['python3', ['scripts/certification/check-visual-baseline.py', 'selftest']] },
  // P0-PROOF: the REAL committed baseline, not its checker's fixtures. A
  // missing or unfingerprinted required platform refuses the release gate.
  { name: 'visual-record-release', command: ['node', ['scripts/check-visual-platforms.mjs', '--release']] },
  // Every REQUIRED platform record, including its resolved font fingerprint:
  // the real committed baseline, never a synthetic fixture.
  ...['linux', 'macos', 'windows'].map((platform) => ({
    name: `visual-${platform}`,
    command: [
      'python3',
      ['scripts/certification/check-visual-baseline.py', '--platform', platform,
       '--file', 'apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json'],
    ],
  })),
  { name: 'contracts-emit', command: ['node', ['scripts/contracts/emit.mjs', 'selftest']] },
  // P2: the REAL checked-in contract digest, not merely the emitter's
  // selftest — a stale generated contract must fail inside prove-all itself.
  { name: 'contracts-verify', command: ['node', ['scripts/contracts/emit.mjs', 'verify', '--print-digest']] },
  { name: 'compiled-proofs-gate', command: ['python3', ['scripts/certification/check-capability-tests-compiled.py', 'selftest']] },
  { name: 'visual-accept', command: ['node', ['scripts/certification/accept-visual-baseline.mjs', 'selftest']] },
];

function runChecks() {
  const results = [];
  let failed = false;
  for (const check of CHECKS) {
    const [command, args] = check.command;
    try {
      execFileSync(command, args, {
        cwd: ROOT,
        stdio: 'pipe',
        timeout: 10 * 60 * 1000,
        maxBuffer: 64 * 1024 * 1024,
      });
      results.push({ name: check.name, status: 'passed' });
    } catch (error) {
      failed = true;
      const tail = `${error.stdout ?? ''}${error.stderr ?? ''}`.slice(-800);
      results.push({ name: check.name, status: 'failed', tail });
    }
  }
  return { results, failed };
}

function main() {
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
  const { results, failed } = runChecks();
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
  const report = {
    schema: 'faktor-prove-all/v2',
    checkout: {
      digest_before: `sha256:${before.digest}`,
      digest_after: `sha256:${after.digest}`,
      stable: before.digest === after.digest && dirtyAfter === null,
      files: before.files,
      clean: dirtyAfter === null,
    },
    checks: results,
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
  console.log(
    `prove-all: PASS (checkout sha256:${before.digest} stable before and after, ${before.files} files, ${results.length} offline checks)`,
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
  process.exit(main());
}
