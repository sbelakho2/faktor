#!/usr/bin/env node
// Programme evidence runner.
//
// Fast, deterministic probes for the requirement registry. A probe is a real
// oracle over the current tree: it either measures a product surface (route,
// CLI command, host contribution) or runs a static/plan check. Heavy lanes
// (real IDEs, live providers, scale curves) add their own evidence through
// scripts/programme/run-lane.mjs; this module never claims their results.

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discoverCliCommands } from '../../scripts/discover-capabilities.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

function read(rel) {
  const path = join(ROOT, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

function walk(rel, filter, out = []) {
  const dir = join(ROOT, rel);
  if (!existsSync(dir)) return out;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === 'node_modules' || entry.name === '.git') continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(relative(ROOT, path), filter, out);
    else if (filter(path)) out.push(path);
  }
  return out;
}

export function findRustFn(name) {
  const needle = new RegExp(`fn\\s+${name.replace(/[$]/g, '\\$')}\\s*\\(`);
  for (const top of ['crates', 'tests']) {
    for (const file of walk(top, (path) => path.endsWith('.rs'))) {
      if (needle.test(readFileSync(file, 'utf8'))) {
        return relative(ROOT, file).split('\\').join('/');
      }
    }
  }
  return null;
}

export function cliBinary() {
  const candidates = [
    process.env.FAKTOR_CLI_BIN,
    resolve(ROOT, 'target/release/faktor-cli'),
    resolve(ROOT, 'target/debug/faktor-cli'),
  ].filter(Boolean);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

/**
 * Execute `faktor-cli <command...> --help` for one discovered command. The
 * oracle is exact: exit 0, help mentions the command's leaf name, and an
 * unknown flag on the same command exits nonzero.
 */
export function probeCliCommand(command, binary = cliBinary()) {
  if (binary === null) {
    return { passed: false, reason: 'no faktor-cli binary available' };
  }
  const argv = command.split('.');
  let help;
  try {
    help = execFileSync(binary, [...argv, '--help'], {
      encoding: 'utf8',
      timeout: 20_000,
      env: { ...process.env, NO_COLOR: '1' },
    });
  } catch (error) {
    return {
      passed: false,
      reason: `help failed for '${command}': ${String(error.stderr ?? error.message).slice(0, 300)}`,
    };
  }
  const leaf = argv[argv.length - 1];
  if (!help.toLowerCase().includes(leaf.toLowerCase())) {
    return { passed: false, reason: `help for '${command}' does not name '${leaf}'` };
  }
  let unknownFlag = 0;
  try {
    execFileSync(binary, [...argv, '--definitely-not-a-flag'], {
      encoding: 'utf8',
      timeout: 20_000,
      stdio: 'pipe',
    });
  } catch (error) {
    unknownFlag = typeof error.status === 'number' ? error.status : 1;
  }
  if (unknownFlag === 0) {
    return { passed: false, reason: `'${command}' accepted an unknown flag` };
  }
  return {
    passed: true,
    observed: { command, help_bytes: Buffer.byteLength(help), unknown_flag_exit: unknownFlag },
  };
}

/** Every discovered CLI command must be executable by name. */
export function probeCliSurface() {
  const binary = cliBinary();
  if (binary === null) {
    return { status: 'deferred', reason: 'no faktor-cli binary; trusted lane builds it' };
  }
  const commands = discoverCliCommands(ROOT);
  const results = commands.map((command) => ({ command, ...probeCliCommand(command, binary) }));
  const failed = results.filter((result) => !result.passed);
  return {
    status: failed.length === 0 ? 'proven' : 'failed',
    results,
    observed: { binary, commands: results.length, failed: failed.slice(0, 10) },
  };
}

/** Static check: every invariant has a real Rust test function. */
export function probeInvariant(row) {
  const unit = row.test_ids.find((id) => !id.endsWith('.mutation'));
  if (!unit) return { passed: false, reason: 'no unit test id' };
  const file = findRustFn(unit);
  return file === null
    ? { passed: false, reason: `unit test '${unit}' not found in crates/** or tests/**` }
    : { passed: true, observed: { unit_test: unit, file } };
}

/** Check a nightly lane actually schedules the scale probe. */
export function probeScaleSchedule() {
  const yaml = read('.woodpecker/trusted/nightly.yaml');
  const hasStep = /^  - name:\s*scale\s*$/m.test(yaml);
  const hasCommand = /scale-probe\.mjs/.test(yaml);
  return hasStep && hasCommand
    ? { passed: true, observed: { lane: 'nightly', step: 'scale' } }
    : { passed: false, reason: 'nightly lane does not schedule scripts/edition8/scale-probe.mjs' };
}
