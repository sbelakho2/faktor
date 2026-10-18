#!/usr/bin/env node
// CLI entry point for change-aware selection.
//
//   node scripts/programme/select-tests.mjs [--mode pull|trusted|nightly|weekly-live|release]
//        [--base <commit>] [--worktree] [--changed-file PATH]... [--out FILE]
//   node scripts/programme/select-tests.mjs selftest
//
// Exit codes: 0 selected; 2 usage; 3 typed refusal (an unmapped changed path;
// callers MUST run the full relevant layer instead of skipping).

import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const module = await import(resolve(ROOT, 'tests/programme/selector.mjs'));

if (process.argv.includes('selftest')) {
  process.exitCode = module.selftest();
} else {
  process.exitCode = module.main();
}
