#!/usr/bin/env node
// Mutation witness for INV-GENERATED-DTO-PARITY: plant a client drift, run
// the real parity gate, and exit 0 ONLY when the gate detects the drift.
// Exits non-zero when the planted drift slips through (or setup fails).
// The restore runs in `finally` BEFORE any exit call: process.exit() does
// not run pending finally blocks, so the exit code is decided first.
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';

const file = 'apps/vscode/src/generated/protocolDto.ts';
const original = readFileSync(file, 'utf8');
let detected = false;
try {
  writeFileSync(file, original + '\n// planted drift for the parity mutation witness\n');
  try {
    execFileSync('node', ['scripts/protocol-codegen.mjs', '--check-clients'], { stdio: 'pipe' });
  } catch {
    detected = true;
  }
} finally {
  writeFileSync(file, original);
}
if (detected) {
  console.log('dto-parity mutation detected by --check-clients');
  process.exit(0);
}
console.error('dto-parity mutation NOT detected: --check-clients passed with a planted client drift');
process.exit(1);
