#!/usr/bin/env node
// Build the per-surface requirement registry from the current tree.
//
//   node scripts/programme/build-registry.mjs             # probe + write
//   node scripts/programme/build-registry.mjs --no-probe  # structure only
//   node scripts/programme/build-registry.mjs --check     # derive, validate,
//                                                         # write status evidence
//
// A new route/command/control/screen/config key/adapter discovered by the
// generator appears here automatically; a stale row disappears on the next
// build. The build never invents a pass: statuses come from executable probes
// or stay deferred/stub.

import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const registry = await import(resolve(ROOT, 'tests/programme/registry.mjs'));

const args = process.argv.slice(2);
if (args.includes('--check')) {
  process.exitCode = await registry.main();
} else {
  const built = await registry.buildRegistry({ dynamic: !args.includes('--no-probe') });
  const written = registry.writeRegistry(built);
  for (const [kind, count] of Object.entries(built.counts.by_kind)) {
    console.log(`registry: kind ${kind}: ${count}`);
  }
  for (const [status, count] of Object.entries(built.counts.by_status)) {
    console.log(`registry: status ${status}: ${count}`);
  }
  console.log(`registry: wrote ${written.registry}`);
}
