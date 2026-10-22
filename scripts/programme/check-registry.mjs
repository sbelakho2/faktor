#!/usr/bin/env node
// Check the committed requirement registry against live discovery and probes.
//
//   node scripts/programme/check-registry.mjs --check
//   node scripts/programme/check-registry.mjs --release
//   node scripts/programme/check-registry.mjs --selftest
//   ... --no-probe   structural only (no daemon/CLI probes)
//
// Exit codes: 0 pass; 1 problems; 2 usage.

import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const registry = await import(resolve(ROOT, 'tests/programme/registry.mjs'));
const wiring = await import(resolve(ROOT, 'tests/programme/checks/wiring.mjs'));

function wiringCheck({ release }) {
  const result = wiring.scan(ROOT);
  for (const warning of result.warnings) console.error(`warning: ${warning}`);
  for (const problem of result.problems) console.error(`problem: ${problem}`);
  console.log(
    `wiring matrix: ${result.counts.commands ?? 0} commands, ${result.counts.settings ?? 0} settings, ${result.counts.routes ?? 0} routes, ${result.counts.configKeys ?? 0} config keys; ${result.problems.length} problems, ${result.warnings.length} warnings`,
  );
  return result.problems.length === 0 && !(release && result.warnings.length > 0) ? 0 : 1;
}

const args = process.argv.slice(2);
if (args.includes('--selftest')) {
  const registryCode = registry.selftest();
  const wiringCode = wiring.selftest();
  process.exitCode = registryCode === 0 && wiringCode === 0 ? 0 : 1;
} else if (args.includes('--release')) {
  const registryCode = await registry.main();
  process.exitCode = registryCode === 0 ? wiringCheck({ release: true }) : registryCode;
} else if (args.includes('--check')) {
  const registryCode = await registry.main();
  process.exitCode = registryCode === 0 ? wiringCheck({ release: false }) : registryCode;
} else {
  console.error('usage: check-registry.mjs --check|--release|--selftest [--no-probe]');
  process.exitCode = 2;
}
