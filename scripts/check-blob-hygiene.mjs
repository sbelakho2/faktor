#!/usr/bin/env node
// Repository blob hygiene gate (act-now row 19, alias of the programme
// scanner): no ELF core, minidump, Mach-O core or Java class, and no
// oversized blob outside the reviewed allowlist, in the tracked tree or in
// any reachable git object. Content sniff, not extension.
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const check = await import(resolve(ROOT, 'tests/programme/checks/core-dumps.mjs'));
if (process.argv.includes('--selftest')) {
  process.exitCode = check.selftest();
} else {
  const result = check.scan({ root: ROOT });
  console.log(`blob hygiene: ${result.passed ? 'PASS' : 'FAIL'}`);
  if (!result.passed) {
    console.error(result.reason);
    process.exitCode = 1;
  }
}
