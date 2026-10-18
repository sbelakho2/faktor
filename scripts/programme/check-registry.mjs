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

const args = process.argv.slice(2);
if (args.includes('--selftest')) {
  process.exitCode = registry.selftest();
} else if (args.includes('--release')) {
  process.exitCode = await registry.main();
} else if (args.includes('--check')) {
  process.exitCode = await registry.main();
} else {
  console.error('usage: check-registry.mjs --check|--release|--selftest [--no-probe]');
  process.exitCode = 2;
}
