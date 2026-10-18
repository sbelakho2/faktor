#!/usr/bin/env node
// CLI wrapper: static ban on lazy task_id() settlement reads.
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const check = await import(resolve(ROOT, 'tests/programme/checks/settlement.mjs'));
if (process.argv.includes('--selftest')) {
  process.exitCode = check.selftest();
} else {
  const result = check.scan({ root: ROOT });
  console.log(`settlement scan: ${result.passed ? 'PASS' : 'FAIL'} (${result.observed?.settlement_reads ?? 0} production reads)`);
  if (!result.passed) {
    console.error(result.reason);
    process.exitCode = 1;
  }
}
