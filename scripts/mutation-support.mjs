// Hermetic mutation-gate support artifacts (audit P2-9 + P1-11).
//
// The isolated mutation scratch copy needs two SUPPORT trees that must never
// be symlinked back into the live checkout (a parallel lane could relink
// them mid-campaign) and must be CONTENT-verified:
//
//   * the CLI is an immutable COPY with its sha256 recorded;
//   * node_modules is a hardlink snapshot (`cp -a -l`; same-filesystem only)
//     with fallback to a FULL COPY — never a symlink — and BOTH source and
//     destination content-digested at provisioning (equal digests), with the
//     destination re-digested after the campaign (detects in-place edits of
//     a hardlinked dependency file);
//   * every digest uses lstat (symlinks are hashed by target string, never
//     followed), reads file contents up to a bound, and FAILS CLOSED on an
//     unreadable entry, a truncation or an entry cap — "unavailable" is a
//     campaign failure, not a degraded success.

import { createHash } from 'node:crypto';
import { cpSync, existsSync, lstatSync, mkdirSync, readFileSync, readlinkSync, readdirSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, relative, sep } from 'node:path';

const MAX_TREE_ENTRIES = 1_000_000;
const MAX_TREE_BYTES = 8 * 1024 * 1024 * 1024;

export function digestFile(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

/**
 * Content digest of a directory tree: paths, modes, symlink TARGET STRINGS
 * and file CONTENTS (bounded). Any unreadable entry, entry overflow or byte
 * overflow throws — never a silently partial digest.
 */
export function digestTree(root, options = {}) {
  const withMetadata = options.metadata !== false;
  const hash = createHash('sha256');
  let entries = 0;
  let bytes = 0;
  const walk = (dir) => {
    const names = readdirSync(dir).sort();
    for (const name of names) {
      const path = join(dir, name);
      const info = lstatSync(path);
      entries += 1;
      if (entries > MAX_TREE_ENTRIES) {
        throw new Error(`support tree exceeds ${MAX_TREE_ENTRIES} entries (fail closed)`);
      }
      hash.update(relative(root, path).split(sep).join('/'));
      if (withMetadata) {
        hash.update(`:${info.mode.toString(8)}:`);
      } else {
        hash.update(':');
      }
      if (info.isSymbolicLink()) {
        hash.update(`symlink=${readlinkSync(path)}`);
      } else if (info.isDirectory()) {
        hash.update('dir');
        hash.update('\0');
        walk(path);
        continue;
      } else if (info.isFile()) {
        const content = readFileSync(path);
        bytes += content.length;
        if (bytes > MAX_TREE_BYTES) {
          throw new Error(`support tree exceeds ${MAX_TREE_BYTES} bytes (fail closed)`);
        }
        hash.update(`file:${content.length}:`);
        hash.update(content);
      } else {
        hash.update('other');
      }
      hash.update('\0');
    }
  };
  walk(root);
  return { digest: hash.digest('hex'), entries, bytes };
}

/**
 * Provision the scratch copy's support trees and content-verify the
 * node_modules snapshot against its source. Returns the recorded support
 * manifest (also written to `<scratch>/.mutation-support.json`).
 */
export function provisionSupport(sourceRoot, scratch) {
  const record = { schema: 'faktor-mutation-support/v2', cli: null, node_modules: null };
  const cli = join(sourceRoot, 'target', 'debug', 'faktor-cli');
  if (existsSync(cli)) {
    const dest = join(scratch, 'target', 'debug', 'faktor-cli');
    mkdirSync(join(scratch, 'target', 'debug'), { recursive: true });
    cpSync(cli, dest);
    try {
      execFileSync('chmod', ['+x', dest]);
    } catch {
      // Windows/unsupported: the destination mode follows the filesystem.
    }
    record.cli = { method: 'copy', sha256: digestFile(dest) };
  }
  const modules = join(sourceRoot, 'apps', 'vscode', 'node_modules');
  if (existsSync(modules)) {
    const dest = join(scratch, 'apps', 'vscode', 'node_modules');
    mkdirSync(join(scratch, 'apps', 'vscode'), { recursive: true });
    let method = null;
    try {
      // Same-filesystem hardlink snapshot: cheap and content-stable because
      // package installers replace files rather than editing in place; the
      // content digests below prove it before and after.
      execFileSync('cp', ['-a', '-l', modules, dest], { stdio: ['ignore', 'ignore', 'pipe'] });
      method = 'hardlink-snapshot';
    } catch {
      // Full copy, still never a symlink.
      // `verbatimSymlinks` keeps RELATIVE link targets relative so the
      // content digest of the snapshot equals the source's.
      cpSync(modules, dest, { recursive: true, verbatimSymlinks: true });
      method = 'copy';
    }
    // Content identity (paths, symlink targets, bytes) across the snapshot:
    // a full copy may normalize permission bits, which is not a content
    // change; the recorded metadata digest below still pins the snapshot.
    const source = digestTree(modules, { metadata: false });
    const snapshot = digestTree(dest, { metadata: false });
    if (source.digest !== snapshot.digest) {
      throw new Error(
        'mutation support snapshot content mismatch (source != destination); refusing a non-hermetic campaign',
      );
    }
    record.node_modules = {
      method,
      digest: snapshot.digest,
      metadata_digest: digestTree(dest).digest,
      entries: snapshot.entries,
      bytes: snapshot.bytes,
    };
  }
  writeFileSync(join(scratch, '.mutation-support.json'), `${JSON.stringify(record, null, 2)}\n`);
  return record;
}

/** Re-digest the snapshot after the campaign (detects in-place hardlink edits). */
export function verifySupportSnapshot(scratch, record) {
  if (!record?.node_modules?.digest) return { ok: true };
  const dest = join(scratch, 'apps', 'vscode', 'node_modules');
  if (!existsSync(dest)) return { ok: false, reason: 'support snapshot disappeared mid-campaign' };
  const after = digestTree(dest, { metadata: false });
  if (after.digest !== record.node_modules.digest) {
    return { ok: false, reason: 'support snapshot content changed mid-campaign' };
  }
  return { ok: true };
}
