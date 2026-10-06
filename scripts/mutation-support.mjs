// Hermetic mutation-gate support artifacts (audit P2-9).
//
// The isolated mutation scratch copy needs two SUPPORT trees that must never
// be copied wholesale (multi-GB): the built CLI the JetBrains gradle smokes
// execute and the VS Code extension's installed dev dependencies. Earlier
// revisions SYMLINKED both back into the real checkout, which let another
// lane relink/replace them mid-campaign (flaky pristine controls, gates
// testing a different compilation state, evidence depending on mutable
// external state). This module snapshots them instead:
//
//   * the CLI is a single immutable COPY with its sha256 recorded;
//   * node_modules is a HARDLINK snapshot (`cp -a -l`; content is stable
//     because installers replace files rather than editing them in place),
//     with a path+size+mtime digest recorded, symlink fallback only when
//     hardlinking is unavailable;
//   * both digests are written to `<scratch>/.mutation-support.json` so the
//     campaign can bind its evidence to exactly the support state it used.

import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readFileSync, readdirSync, statSync, symlinkSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, relative, sep } from 'node:path';

function digestFile(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

/** Bounded path+size+mtime digest of a directory tree (immutability evidence). */
function digestTree(root) {
  const hash = createHash('sha256');
  let entries = 0;
  const walk = (dir) => {
    let names;
    try {
      names = readdirSync(dir).sort();
    } catch {
      return;
    }
    for (const name of names) {
      const path = join(dir, name);
      let info;
      try {
        info = statSync(path);
      } catch {
        continue;
      }
      if (entries > 500_000) return;
      entries += 1;
      if (info.isDirectory()) {
        walk(path);
      } else {
        hash.update(relative(root, path).split(sep).join('/'));
        hash.update(`:${info.size}:${info.mtimeMs}`);
        hash.update('\0');
      }
    }
  };
  walk(root);
  return { digest: hash.digest('hex'), entries };
}

/**
 * Provision the scratch copy's support trees. Returns the recorded support
 * manifest (also written to `<scratch>/.mutation-support.json`).
 */
export function provisionSupport(sourceRoot, scratch) {
  const record = { schema: 'faktor-mutation-support/v1', cli: null, node_modules: null };
  const cli = join(sourceRoot, 'target', 'debug', 'faktor-cli');
  if (existsSync(cli)) {
    const dest = join(scratch, 'target', 'debug', 'faktor-cli');
    mkdirSync(join(scratch, 'target', 'debug'), { recursive: true });
    cpSync(cli, dest);
    // The copy is the executable the gates run; make the mode explicit.
    try {
      execFileSync('chmod', ['+x', dest]);
    } catch {
      // Windows/unsupported: the spawn mode follows the destination fs.
    }
    record.cli = { method: 'copy', sha256: digestFile(dest) };
  }
  const modules = join(sourceRoot, 'apps', 'vscode', 'node_modules');
  if (existsSync(modules)) {
    const dest = join(scratch, 'apps', 'vscode', 'node_modules');
    mkdirSync(join(scratch, 'apps', 'vscode'), { recursive: true });
    try {
      execFileSync('cp', ['-a', '-l', modules, dest], { stdio: ['ignore', 'ignore', 'pipe'] });
      record.node_modules = { method: 'hardlink-snapshot', ...digestTree(dest) };
    } catch {
      try {
        cpSync(modules, dest, { recursive: true });
        record.node_modules = { method: 'copy', ...digestTree(dest) };
      } catch {
        try {
          symlinkSync(modules, dest);
          record.node_modules = { method: 'symlink-fallback', digest: null };
        } catch {
          record.node_modules = { method: 'unavailable', digest: null };
        }
      }
    }
  }
  writeFileSync(join(scratch, '.mutation-support.json'), `${JSON.stringify(record, null, 2)}\n`);
  return record;
}
