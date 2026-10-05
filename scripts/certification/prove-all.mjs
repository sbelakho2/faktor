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
import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = join(ROOT, 'target/certification/prove-all.json');

/** Content hash of every tracked worktree file (committed or not). */
export function checkoutDigest(root) {
  const files = execFileSync('git', ['ls-files', '-z'], { cwd: root, encoding: 'utf8' })
    .split('\0')
    .filter((file) => file.length > 0)
    .sort();
  const hash = createHash('sha256');
  for (const file of files) {
    let bytes;
    try {
      bytes = readFileSync(join(root, file));
    } catch {
      // A tracked file deleted from the worktree is part of the checked-out
      // state: hash the deletion marker, never skip it.
      hash.update(`${file}\0<deleted>\0`);
      continue;
    }
    hash.update(`${file}\0`);
    hash.update(bytes);
    hash.update('\0');
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
  { name: 'coverage', command: ['node', ['scripts/check-invariant-coverage.mjs', '--check']] },
  { name: 'workflow-graph', command: ['node', ['scripts/certification/check-workflow-graph.mjs', '--check']] },
  { name: 'publisher', command: ['node', ['scripts/certification/publish-status.mjs', 'selftest']] },
  { name: 'soak-sampler', command: ['python3', ['scripts/certification/soak-convergence.py', 'selftest']] },
  { name: 'visual-record', command: ['python3', ['scripts/certification/check-visual-baseline.py', 'selftest']] },
  { name: 'contracts-emit', command: ['node', ['scripts/contracts/emit.mjs', 'selftest']] },
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
  const dirty = dirtyCheckout(ROOT);
  if (dirty !== null) {
    console.error(`prove-all: FAIL — the checkout is dirty (${dirty}); certifying bytes must be committed`);
    return 1;
  }
  const { digest, files } = checkoutDigest(ROOT);
  const { results, failed } = runChecks();
  const report = {
    schema: 'faktor-prove-all/v1',
    checkout: { digest: `sha256:${digest}`, files, clean: true },
    checks: results,
    not_proven_here: [
      'platform certificate authentication (ci/faktor/trusted-certified-{linux,darwin,windows} signatures) — publish-status aggregate lane',
      'mutation campaign tree barrier and isolated-copy execution — check-invariants --mutations lane',
      'quiescent convergence (RSS/FD/children/queues/WAL/temp/CAS/latency) — soak lanes',
      'real VS Code Extension Host and JetBrains packaged-ZIP host matrices — vscode-e2e / jetbrains-smoke lanes',
    ],
  };
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${JSON.stringify(report, null, 2)}\n`);
  if (failed) {
    for (const result of results.filter((entry) => entry.status === 'failed')) {
      console.error(`prove-all: check ${result.name} FAILED\n${result.tail}`);
    }
    console.error(`prove-all: FAIL (checkout sha256:${digest}, ${files} files)`);
    return 1;
  }
  console.log(`prove-all: PASS (checkout sha256:${digest}, ${files} files, ${results.length} offline checks)`);
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
