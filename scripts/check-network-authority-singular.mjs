#!/usr/bin/env node
// Singular-authority check (audit P2, mechanical): production egress
// authorization is the canonical destination/DNS authority. The retired
// `NetworkPolicy::allows` raw URL/prefix matcher must never return to an
// authoritative (or any) role:
//
//   * no `NetworkPolicy::allows` call anywhere under crates/;
//   * no `fn allows`/`fn authority_allows`/`fn split_authority` matcher in
//     the pure core capability type.
//
// Exits non-zero with the offending file:line list, so a reintroduction is
// caught by the release gate, not by review.
import { readdirSync, readFileSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function rustFiles(dir, out = []) {
  for (const name of readdirSync(dir)) {
    if (name === 'target' || name === 'node_modules' || name === '.git') continue;
    const path = join(dir, name);
    if (statSync(path).isDirectory()) rustFiles(path, out);
    else if (name.endsWith('.rs')) out.push(path);
  }
  return out;
}

const violations = [];
for (const file of rustFiles(join(ROOT, 'crates'))) {
  const text = readFileSync(file, 'utf8');
  const rel = relative(ROOT, file);
  text.split('\n').forEach((line, index) => {
    const at = `${rel}:${index + 1}`;
    if (/\bNetworkPolicy::allows\b/.test(line)) {
      violations.push(`${at}: NetworkPolicy::allows must not be called`);
    }
    if (rel === 'crates/core/src/capability.rs') {
      if (/\bfn\s+allows\s*\(/.test(line)) {
        violations.push(`${at}: the retired raw matcher (fn allows) must not exist`);
      }
      if (/\bfn\s+authority_allows\s*\(|\bfn\s+split_authority\s*\(/.test(line)) {
        violations.push(`${at}: raw network parsing helper must not exist`);
      }
    }
  });
}

if (violations.length > 0) {
  for (const violation of violations) console.error(`network-authority: ${violation}`);
  process.exit(1);
}
console.log('network-authority: singular (canonical destination authority only)');
