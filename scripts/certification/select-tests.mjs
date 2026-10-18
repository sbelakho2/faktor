#!/usr/bin/env node
// Compatibility entry point for the change-aware selector.
//
// The authoritative implementation is tests/programme/selector.mjs; this
// wrapper accepts the certification spelling (`--lane`, `--head`) and maps it
// onto `--mode`, so local/CI callers cannot diverge.
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const selector = await import(resolve(ROOT, 'tests/programme/selector.mjs'));

const args = process.argv.slice(2);
const mapped = [];
for (let index = 0; index < args.length; index += 1) {
  if (args[index] === '--lane' && index + 1 < args.length) {
    mapped.push('--mode', args[index + 1]);
    index += 1;
  } else if (args[index] === '--head') {
    index += 1; // informational; the selector reads the real HEAD
  } else {
    mapped.push(args[index]);
  }
}
process.argv = [process.argv[0], process.argv[1], ...mapped];
process.exitCode = mapped.includes('selftest') ? selector.selftest() : selector.main();
