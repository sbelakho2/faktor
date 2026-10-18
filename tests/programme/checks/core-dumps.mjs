#!/usr/bin/env node
// Core-dump hygiene scan (Report.pdf row 19 / act-now).
//
// Fails when a large crash artifact is reachable from the tree or from any
// git ref. Detection sniffs CONTENT, not only the extension:
//   * ELF objects with e_type = ET_CORE,
//   * Windows minidumps (`MDMP`),
//   * Mach-O objects with filetype = MH_CORE.
// Independently, any blob above the generic size cap that is not on the
// reviewed allowlist fails ("cap the size of any blob not on an allowlist").
//
// History is scanned through `git rev-list --objects --all` +
// `git cat-file --batch-check`, so a dump deleted from HEAD but still
// reachable from a branch/tag is caught.

import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');

export const CORE_NAME = /(^|\/)(core(\.[0-9]+)?|core\.[A-Za-z0-9_.-]+|[^/]+\.core|[^/]+\.dmp|[^/]+\.mdmp)$/i;
export const CORE_SIZE_LIMIT = 8 * 1024 * 1024;
export const GENERIC_SIZE_LIMIT = 64 * 1024 * 1024;

// Reviewed large artifacts (source PDFs and committed visual baselines).
export const SIZE_ALLOWLIST = Object.freeze([
  'Faktor_IDE_UI_UX_3.pdf',
  'Report.pdf',
  'apps/vscode/scripts/baselines/edition5/darwin/contact.png',
  'apps/vscode/scripts/baselines/edition8/macos.json',
]);

/** Content sniff: returns 'elf-core' | 'minidump' | 'mach-o-core' | null. */
export function sniffCore(bytes) {
  if (!bytes || bytes.length < 18) return null;
  if (
    bytes[0] === 0x7f &&
    bytes[1] === 0x45 &&
    bytes[2] === 0x4c &&
    bytes[3] === 0x46
  ) {
    const little = bytes[5] === 1;
    const type = little
      ? bytes[16] | (bytes[17] << 8)
      : (bytes[16] << 8) | bytes[17];
    if (type === 4) return 'elf-core';
  }
  if (bytes.slice(0, 4).toString('latin1') === 'MDMP') return 'minidump';
  if (bytes[0] === 0xca && bytes[1] === 0xfe && bytes[2] === 0xba && bytes[3] === 0xbe) {
    return 'java-class';
  }
  const magic = bytes.readUInt32BE(0);
  if ([0xfeedface, 0xcefaedfe, 0xfeedfacf, 0xcffaedfe].includes(magic)) {
    const little = magic === 0xcefaedfe || magic === 0xcffaedfe;
    const filetype = little
      ? bytes.readUInt32LE(12)
      : bytes.readUInt32BE(12);
    if (filetype === 4) return 'mach-o-core';
  }
  return null;
}

function walk(root, dir, out) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    if (['.git', 'target', 'target-linux-amd64', 'node_modules', '.gradle', 'build', 'out'].includes(entry.name)) {
      continue;
    }
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(root, path, out);
    } else if (entry.isFile()) {
      out.push(relative(root, path).split('\\').join('/'));
    }
  }
}

function readHead(path, length) {
  try {
    const data = readFileSync(path);
    return data.subarray(0, length);
  } catch {
    return null;
  }
}

function git(root, args, options = {}) {
  return execFileSync('git', ['-C', root, ...args], {
    encoding: options.encoding ?? 'utf8',
    maxBuffer: options.maxBuffer ?? 256 * 1024 * 1024,
    stdio: options.stdio,
  });
}

/**
 * Tracked-tree scan (the git index is what a clone materializes). Pass
 * `untracked: true` to also inspect local working-tree artifacts; the
 * default mirrors the report's "blob" wording so a developer's scratch dump
 * does not fail a clean-checkout gate.
 */
export function scanTree({ root = ROOT, untracked = false } = {}) {
  let files;
  if (!untracked && existsSync(join(root, '.git'))) {
    try {
      files = git(root, ['ls-files', '-z'])
        .split('\0')
        .filter(Boolean)
        .map((path) => path.split('\\').join('/'));
    } catch {
      files = [];
    }
  } else {
    files = [];
    walk(root, root, files);
  }
  const findings = [];
  for (const rel of files) {
    const path = join(root, rel);
    let size;
    try {
      size = statSync(path).size;
    } catch {
      continue;
    }
    if (size < CORE_SIZE_LIMIT) continue;
    const head = readHead(path, 32);
    const kind = sniffCore(head);
    const named = CORE_NAME.test(rel);
    if (kind !== null) {
      findings.push({ scope: 'tree', path: rel, bytes: size, kind });
    } else if (named) {
      findings.push({ scope: 'tree', path: rel, bytes: size, kind: 'core-named' });
    } else if (size >= GENERIC_SIZE_LIMIT && !SIZE_ALLOWLIST.includes(rel)) {
      findings.push({ scope: 'tree', path: rel, bytes: size, kind: 'oversized-unallowlisted' });
    }
  }
  return findings;
}

/** Reachable-history scan over every ref. */
export function scanHistory({ root = ROOT } = {}) {
  const findings = [];
  let objects;
  try {
    objects = git(root, ['rev-list', '--objects', '--all']);
  } catch {
    return findings;
  }
  const entries = objects
    .split('\n')
    .filter(Boolean)
    .map((line) => {
      const space = line.indexOf(' ');
      return space === -1
        ? { sha: line, path: '' }
        : { sha: line.slice(0, space), path: line.slice(space + 1) };
    });
  if (entries.length === 0) return findings;
  const shas = entries.map((entry) => entry.sha).join('\n');
  let sizes;
  try {
    sizes = execFileSync('git', ['-C', root, 'cat-file', '--batch-check=%(objectname) %(objecttype) %(objectsize)'], {
      input: `${shas}\n`,
      encoding: 'utf8',
      maxBuffer: 512 * 1024 * 1024,
    });
  } catch {
    return findings;
  }
  const sizeBySha = new Map();
  for (const line of sizes.split('\n')) {
    const [sha, type, size] = line.split(' ');
    if (type === 'blob') sizeBySha.set(sha, Number(size));
  }
  for (const entry of entries) {
    const size = sizeBySha.get(entry.sha) ?? 0;
    if (size < CORE_SIZE_LIMIT) continue;
    const named = CORE_NAME.test(entry.path);
    if (!named && size < GENERIC_SIZE_LIMIT) continue;
    let head = null;
    try {
      const proc = spawnSync('git', ['-C', root, 'cat-file', 'blob', entry.sha], {
        maxBuffer: 4096,
      });
      head = proc.stdout && proc.stdout.length > 0 ? proc.stdout.subarray(0, 32) : null;
    } catch {
      head = null;
    }
    const kind = sniffCore(head);
    if (kind !== null) {
      findings.push({ scope: 'history', path: entry.path, sha: entry.sha, bytes: size, kind });
    } else if (named) {
      findings.push({ scope: 'history', path: entry.path, sha: entry.sha, bytes: size, kind: 'core-named' });
    } else if (size >= GENERIC_SIZE_LIMIT && !SIZE_ALLOWLIST.includes(entry.path)) {
      findings.push({
        scope: 'history',
        path: entry.path,
        sha: entry.sha,
        bytes: size,
        kind: 'oversized-unallowlisted',
      });
    }
  }
  return findings;
}

export function scan({ root = ROOT, history = true, untracked = false } = {}) {
  if (!existsSync(join(root, '.git')) && history) {
    history = false;
  }
  const tree = scanTree({ root, untracked });
  const reachable = history ? scanHistory({ root }) : [];
  const findings = [...tree, ...reachable];
  return {
    passed: findings.length === 0,
    observed: {
      tree_findings: tree.length,
      history_findings: reachable.length,
      findings,
      limits: { core_bytes: CORE_SIZE_LIMIT, generic_bytes: GENERIC_SIZE_LIMIT },
      allowlist: SIZE_ALLOWLIST,
    },
    reason:
      findings.length === 0
        ? undefined
        : `large crash artifact(s)/unallowlisted blobs: ${findings
            .slice(0, 8)
            .map((finding) => `${finding.scope}:${finding.path} (${finding.bytes} bytes, ${finding.kind})`)
            .join(', ')}`,
  };
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`core-dump selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`core-dump selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const elf = Buffer.alloc(64);
  elf[0] = 0x7f;
  elf[1] = 0x45;
  elf[2] = 0x4c;
  elf[3] = 0x46;
  elf[4] = 2;
  elf[5] = 1;
  elf.writeUInt16LE(4, 16);
  check('ELF ET_CORE is detected', sniffCore(elf) === 'elf-core');
  elf.writeUInt16LE(2, 16);
  check('ELF ET_EXEC is not a core', sniffCore(elf) === null);
  const mdmp = Buffer.from('MDMPxxxxxxxxxxxxxxxx');
  check('minidump is detected', sniffCore(mdmp) === 'minidump');
  check('Java class is detected', sniffCore(Buffer.from([0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 52, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])) === 'java-class');
  const mach = Buffer.alloc(64);
  mach.writeUInt32LE(0xfeedfacf, 0);
  mach.writeUInt32LE(4, 12);
  check('Mach-O MH_CORE is detected', sniffCore(mach) === 'mach-o-core');

  // Synthetic git repo: a large ELF core committed then deleted still fails.
  const tmp = execFileSync('mktemp', ['-d'], { encoding: 'utf8' }).trim();
  try {
    execFileSync('git', ['-C', tmp, 'init', '-q']);
    execFileSync('git', ['-C', tmp, 'config', 'user.email', 'probe@example.invalid']);
    execFileSync('git', ['-C', tmp, 'config', 'user.name', 'probe']);
    const core = Buffer.alloc(CORE_SIZE_LIMIT + 1);
    core[0] = 0x7f;
    core[1] = 0x45;
    core[2] = 0x4c;
    core[3] = 0x46;
    core[5] = 1;
    core.writeUInt16LE(4, 16);
    writeFileSync(join(tmp, 'crash.core'), core);
    execFileSync('git', ['-C', tmp, 'add', 'crash.core']);
    execFileSync('git', ['-C', tmp, 'commit', '-qm', 'dump']);
    rmSync(join(tmp, 'crash.core'));
    execFileSync('git', ['-C', tmp, 'commit', '-qam', 'remove dump', '--allow-empty']);
    const found = scanHistory({ root: tmp });
    check('a dump deleted from HEAD but reachable in history fails', found.length === 1, JSON.stringify(found));
    check('history finding names the blob and kind', found[0]?.kind === 'elf-core');
  } finally {
    execFileSync('rm', ['-rf', tmp]);
  }
  if (failures > 0) {
    console.error(`core-dump selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('core-dump selftest: PASS');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  if (process.argv.includes('--selftest')) {
    process.exitCode = selftest();
  } else {
    const result = scan({ root: ROOT });
    console.log(
      `core-dump scan: ${result.passed ? 'PASS' : 'FAIL'} (tree=${result.observed.tree_findings}, history=${result.observed.history_findings})`,
    );
    if (!result.passed) {
      console.error(result.reason);
      process.exitCode = 1;
    }
  }
}
