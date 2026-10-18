#!/usr/bin/env node
// CLI wrapper: tree/history core-dump and oversized-blob scanner.
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const check = await import(resolve(ROOT, 'tests/programme/checks/core-dumps.mjs'));
if (process.argv.includes('--selftest')) {
  process.exitCode = check.selftest();
} else {
  const result = check.scan({ root: ROOT });
  console.log(`core-dump scan: ${result.passed ? 'PASS' : 'FAIL'} (tree=${result.observed.tree_findings}, history=${result.observed.history_findings})`);
  if (!result.passed) {
    console.error(result.reason);
    process.exitCode = 1;
  }
}
