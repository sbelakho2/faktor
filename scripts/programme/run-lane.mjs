#!/usr/bin/env node
// Execute one programme lane and write machine-readable evidence.
//
//   node scripts/programme/run-lane.mjs --lane pull [--changed-file PATH]...
//        [--base <commit>] [--print-plan]
//   node scripts/programme/run-lane.mjs --selftest
//   node scripts/programme/run-lane.mjs --lane nightly --lane weekly-live ...
//
// Multiple --lane flags run sequentially; every lane writes
// target/certification/programme/lanes/<lane>.json. Live lanes refuse typed
// without their operator configuration; nothing is silently skipped.

import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const lanes = await import(resolve(ROOT, 'tests/programme/lanes.mjs'));

const args = process.argv.slice(2);
if (args.includes('--selftest')) {
  process.exitCode = lanes.selftest();
} else {
  const requested = [];
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] === '--lane' && index + 1 < args.length) requested.push(args[index + 1]);
  }
  if (requested.length === 0) {
    console.error('usage: run-lane.mjs --lane <pull|trusted|nightly|weekly-live|release> [--print-plan]');
    process.exitCode = 2;
  } else {
    let exitCode = 0;
    for (const lane of requested) {
      if (!lanes.LANE_PLANS[lane]) {
        console.error(`run-lane: unknown lane '${lane}'`);
        exitCode = 2;
        continue;
      }
      // main() in lanes.mjs reads argv; drive the exported helpers directly.
      const changed = [];
      for (let index = 0; index < args.length; index += 1) {
        if (args[index] === '--changed-file' && index + 1 < args.length) {
          changed.push(args[index + 1]);
        }
      }
      const base = args.includes('--base') ? args[args.indexOf('--base') + 1] : null;
      if (args.includes('--print-plan')) {
        const plan = lanes.lanePlan(lane, { changedFiles: changed, base });
        process.stdout.write(`${JSON.stringify(plan, null, 2)}\n`);
        continue;
      }
      const document = lanes.runLane(lane, { changedFiles: changed, base });
      if (document.status !== 'passed') exitCode = 1;
    }
    process.exitCode = exitCode;
  }
}
