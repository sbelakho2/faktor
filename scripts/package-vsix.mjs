#!/usr/bin/env node
// Faktor VSIX packaging — the ONE reproducible flow (audit item 17).
//
// Usage:
//   node scripts/package-vsix.mjs              # compile, stamp build-info, package
//   node scripts/package-vsix.mjs --no-build   # stamp + package the current out/
//   node scripts/package-vsix.mjs --out DIR    # write the .vsix into DIR
//
// This REPLACES every copy-over-version-dir install habit: exactly one
// command produces the artifact (`apps/vscode/out/faktor.faktor-<version>.vsix`),
// stamped with the build provenance the Diagnostics surface reads. The
// packager is the LOCKFILE-pinned `@vscode/vsce` from
// apps/vscode/node_modules (the exact devDependency version in
// apps/vscode/package.json + package-lock.json) — never an npx download,
// never a global binary.
//
// Build provenance (item 17): before packaging, `apps/vscode/build-info.json`
// is (re)written with the git commit (null in an uncommitted tree — never
// guessed), the packaging timestamp, and the sha256 of every shipped
// extension JS/CSS asset plus package.json. The extension reads this file at
// runtime, re-hashes the SAME files on disk, and reports in Diagnostics
// whether the installed bytes still match the packaged build.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const EXT = join(ROOT, 'apps', 'vscode');

function fail(message) {
  console.error(`package-vsix: ${message}`);
  process.exit(1);
}

const argv = process.argv.slice(2);
const noBuild = argv.includes('--no-build');
const outIndex = argv.indexOf('--out');
const outDir = outIndex !== -1 ? argv[outIndex + 1] : null;

// 1. The lockfile-pinned packager (never a downloaded/unpinned vsce).
let vsceManifest;
try {
  vsceManifest = JSON.parse(
    readFileSync(join(EXT, 'node_modules', '@vscode', 'vsce', 'package.json'), 'utf8'),
  );
} catch {
  fail(
    'the lockfile-pinned @vscode/vsce is missing — run `npm ci` inside apps/vscode ' +
      '(packaging never downloads tools ad hoc)',
  );
}
if (vsceManifest.version !== '4.0.0') {
  // The flow is pinned to the reviewed version; a different installed
  // version is a loud refusal, never an implicit upgrade.
  fail(
    `installed @vscode/vsce is ${vsceManifest.version}, expected the pinned 4.0.0 ` +
      '(update the pin and this check in one reviewed change)',
  );
}
const vsceBin = join(EXT, 'node_modules', '@vscode', 'vsce', 'vsce');

// 2. Compile the TypeScript (unless explicitly skipped).
if (!noBuild) {
  const tsc = spawnSync('npm', ['run', 'build'], { cwd: EXT, stdio: 'inherit' });
  if (tsc.status !== 0) {
    fail('the TypeScript build failed; nothing was packaged');
  }
}

// 3. The build provenance stamp.
function gitCommit() {
  const result = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' });
  if (result.status !== 0) {
    return null;
  }
  const dirty = spawnSync('git', ['status', '--porcelain'], { cwd: ROOT, encoding: 'utf8' });
  const commit = result.stdout.trim();
  return dirty.status === 0 && dirty.stdout.trim().length > 0 ? `${commit}-dirty` : commit;
}

function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

/** Every shipped JS/CSS asset plus package.json (the Diagnostics surface). */
function assetList() {
  const assets = [];
  for (const dir of ['media', 'out']) {
    for (const name of readdirSync(join(EXT, dir)).sort()) {
      if (/\.(js|css)$/.test(name)) {
        assets.push(`${dir}/${name}`);
      }
    }
  }
  assets.push('package.json');
  return assets;
}

const manifest = JSON.parse(readFileSync(join(EXT, 'package.json'), 'utf8'));
const buildInfo = {
  schema: 'faktor-vscode-build-info/v1',
  version: manifest.version,
  commit: gitCommit(),
  packagedAt: new Date().toISOString(),
  assets: Object.fromEntries(assetList().map((rel) => [rel, sha256File(join(EXT, rel))])),
};
writeFileSync(join(EXT, 'build-info.json'), `${JSON.stringify(buildInfo, null, 2)}\n`);
console.log(
  `package-vsix: build-info stamped (commit ${buildInfo.commit ?? 'uncommitted'}, ` +
    `${Object.keys(buildInfo.assets).length} assets)`,
);

// 4. Package through the pinned vsce.
const vsceArgs = ['package', '--no-dependencies', '--out', outDir ?? join(EXT, 'out')];
if (outIndex !== -1 && argv[outIndex + 2] !== undefined) {
  // nothing further; --out takes the next token only
}
const vsce = spawnSync('node', [vsceBin, ...vsceArgs], { cwd: EXT, stdio: 'inherit' });
if (vsce.status !== 0) {
  fail('vsce package failed');
}
console.log('package-vsix: OK — install with `code --install-extension <the produced .vsix>`');
