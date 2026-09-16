#!/usr/bin/env node
// Assert that a packaged VSIX is self-contained: the Faktor-owned chat panel
// (out/*.js + media/chat.js, media/chat.css, media/composer-state.js,
// media/faktor.svg) must be inside the archive, and NO vendored upstream
// closure / message-ABI bridge may ship. When an extraction directory is
// given, the packaged panel files are checked for the strict-CSP and
// marker invariants the runtime relies on.
//
// Usage:
//   node scripts/verify-vsix.mjs <vsix> [--min-media-files N]
//   node scripts/verify-vsix.mjs <vsix> --extract-dir <extension-dir>
//   node scripts/verify-vsix.mjs <vsix> --ide-load
//
// --ide-load installs the VSIX into an isolated VS Code extensions directory
// when a `code` CLI harness is available. When none is available it records an
// explicit skip (never a silent pass) in
// target/certification/vsix-ide-load.json and exits 0.

import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const APP_ROOT = resolve(SCRIPT_DIR, '..');
const REPO_ROOT = resolve(APP_ROOT, '..', '..');
const APP_MANIFEST = join(APP_ROOT, 'package.json');

/**
 * The Faktor-owned panel surface is derived from the source tree itself:
 * every `src/*.ts` compiles to one `out/*.js`, and `media/` ships verbatim.
 * The packaged extension may carry NOTHING else under those two prefixes,
 * so any extra artifact (a vendored closure, a bridge output, a stray
 * bundle) fails without the check having to name it.
 */
function expectedPanelFiles() {
  const sources = readdirSync(join(APP_ROOT, 'src'))
    .filter((name) => name.endsWith('.ts'))
    .map((name) => `out/${name.replace(/\.ts$/, '.js')}`);
  const media = readdirSync(join(APP_ROOT, 'media'))
    .filter((name) => statSync(join(APP_ROOT, 'media', name)).isFile())
    .map((name) => `media/${name}`);
  return [...sources, ...media].sort();
}

const PANEL_FILES = expectedPanelFiles();

/**
 * The panel protocol markers: the webview consumes these inbound message
 * kinds and posts these actions; the panel is useless if any vanished.
 */
const PANEL_MARKERS = [
  'snapshot',
  'evidence',
  'startResult',
  'notice',
  'sendGoal',
  'agentControl',
  'tournamentControl',
  'retrieveEvidence',
];

const DEFAULT_MIN_MEDIA_FILES = 4;

let passed = 0;
let failed = 0;

function check(condition, label) {
  if (condition) {
    passed += 1;
    console.log(`PASS  ${label}`);
  } else {
    failed += 1;
    console.error(`FAIL  ${label}`);
  }
}

function unzip(vsix, args) {
  return execFileSync('unzip', args, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
}

function listEntries(vsix) {
  const listing = unzip(vsix, ['-l', vsix]);
  console.log(listing.trimEnd());
  const entries = [];
  for (const line of listing.split(/\r?\n/)) {
    const match = line.match(/^\s*(\d+)\s+\S+\s+\S+\s+(.+?)\s*$/);
    if (match) {
      entries.push({ size: Number(match[1]), name: match[2] });
    }
  }
  return entries;
}

function archiveChecks(vsix, minMediaFiles) {
  const entries = listEntries(vsix);
  const names = new Set(entries.map((entry) => entry.name));
  check(names.has('extension/out/extension.js'), 'VSIX contains extension/out/extension.js');
  for (const relative of PANEL_FILES) {
    check(names.has(`extension/${relative}`), `VSIX contains ${relative}`);
  }
  const mediaFiles = entries.filter((entry) => entry.name.startsWith('extension/media/'));
  check(
    mediaFiles.length >= minMediaFiles,
    `VSIX has >= ${minMediaFiles} files under extension/media (found ${mediaFiles.length})`,
  );
  const allowed = new Set(PANEL_FILES.map((relative) => `extension/${relative}`));
  const extras = entries
    .map((entry) => entry.name)
    .filter(
      (name) => name.startsWith('extension/media/') || name.startsWith('extension/out/'),
    )
    .filter((name) => !allowed.has(name) && !name.endsWith('/'));
  check(
    extras.length === 0,
    `VSIX carries exactly the Faktor-owned panel surface (unexpected: ${extras.join(', ') || 'none'})`,
  );
  return mediaFiles.length;
}

function extractedChecks(extensionDir) {
  for (const relative of PANEL_FILES) {
    check(existsSync(join(extensionDir, relative)), `packaged extension has ${relative}`);
  }
  for (const prefix of ['media', 'out']) {
    const directory = join(extensionDir, prefix);
    if (!existsSync(directory)) {
      check(false, `packaged ${prefix}/ directory is present`);
      continue;
    }
    const expected = PANEL_FILES.filter((relative) => relative.startsWith(`${prefix}/`))
      .map((relative) => relative.slice(prefix.length + 1))
      .sort();
    const actual = readdirSync(directory)
      .filter((name) => statSync(join(directory, name)).isFile())
      .sort();
    check(
      JSON.stringify(actual) === JSON.stringify(expected),
      `packaged ${prefix}/ ships exactly the Faktor-owned panel files (found: ${actual.join(', ')})`,
    );
  }

  const built = join(extensionDir, 'out', 'webview.js');
  if (existsSync(built)) {
    const source = readFileSync(built, 'utf8');
    check(
      source.includes("'media'") && source.includes("'chat.js'"),
      'compiled webview resolves the Faktor-owned media/chat.js panel',
    );
    check(
      !source.includes('FAKTOR_UI_BUNDLE') && !/'\.\.',\s*'\.\.'/.test(source),
      'compiled webview has no bundle override or checkout escape',
    );
    for (const marker of ["default-src 'none'", "script-src 'nonce-", "connect-src 'none'"]) {
      check(source.includes(marker), `compiled webview keeps the strict CSP marker ${marker}`);
    }
  }

  const panelJs = join(extensionDir, 'media', 'chat.js');
  if (!existsSync(panelJs)) {
    check(false, 'panel script media/chat.js is present for marker verification');
    return;
  }
  const source = readFileSync(panelJs, 'utf8');
  const missing = PANEL_MARKERS.filter((marker) => !source.includes(marker));
  check(
    missing.length === 0,
    `Faktor panel consumes/posts the documented message kinds (missing: ${missing.join(', ') || 'none'})`,
  );
  check(
    !source.includes('acquireVsCodeApi') || source.includes('acquireVsCodeApi()'),
    'panel acquires exactly the VS Code API handle',
  );
}

function writeIdeRecord(record) {
  const outDir = join(REPO_ROOT, 'target', 'certification');
  mkdirSync(outDir, { recursive: true });
  const path = join(outDir, 'vsix-ide-load.json');
  writeFileSync(path, `${JSON.stringify(record, null, 2)}\n`);
  console.log(`[ide-load] recorded ${path}: ${record.status}`);
}

function findCodeCli() {
  if (process.env.VSCODE_CLI !== undefined && process.env.VSCODE_CLI.length > 0) {
    return existsSync(process.env.VSCODE_CLI) ? process.env.VSCODE_CLI : null;
  }
  try {
    const found = execFileSync('sh', ['-c', 'command -v code'], { encoding: 'utf8' }).trim();
    return found.length > 0 ? found : null;
  } catch {
    return null;
  }
}

function ideLoad(vsix) {
  const code = findCodeCli();
  if (code === null) {
    const reason =
      'no `code` CLI on PATH (VS Code harness unavailable on this runner); ' +
      'the required job still ran the copied-extension assertions';
    console.log(`SKIP  IDE launch: ${reason}`);
    writeIdeRecord({ status: 'skipped', reason, vsix: resolve(vsix) });
    return;
  }
  const work = mkdtempSync(join(tmpdir(), 'faktor-vsix-ide-'));
  const extensionsDir = join(work, 'extensions');
  const userDir = join(work, 'user-data');
  try {
    execFileSync(code, ['--version'], { encoding: 'utf8' });
    execFileSync(
      code,
      [
        '--extensions-dir',
        extensionsDir,
        '--user-data-dir',
        userDir,
        '--install-extension',
        resolve(vsix),
        '--force',
      ],
      { encoding: 'utf8' },
    );
    const listed = execFileSync(
      code,
      ['--extensions-dir', extensionsDir, '--user-data-dir', userDir, '--list-extensions', '--show-versions'],
      { encoding: 'utf8' },
    );
    console.log(listed.trimEnd());
    const manifest = JSON.parse(readFileSync(APP_MANIFEST, 'utf8'));
    const extensionId = `${manifest.publisher}.${manifest.name}`.replace(/\./g, '\\.');
    check(
      new RegExp(`^${extensionId}@`, 'm').test(listed),
      `VS Code harness loaded the packaged extension ${manifest.publisher}.${manifest.name} (${code})`,
    );
    writeIdeRecord({ status: failed === 0 ? 'loaded' : 'failed', reason: '', vsix: resolve(vsix) });
  } catch (error) {
    writeIdeRecord({
      status: 'failed',
      reason: String(error && error.message ? error.message : error),
      vsix: resolve(vsix),
    });
    console.error(`FAIL  IDE launch via ${code}: ${error && error.message ? error.message : error}`);
    failed += 1;
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

function parseArgs(argv) {
  const options = { minMediaFiles: DEFAULT_MIN_MEDIA_FILES, extractDir: null, ideLoad: false, vsix: null };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--extract-dir') {
      options.extractDir = resolve(argv[(i += 1)] ?? '');
    } else if (arg === '--min-media-files' || arg === '--min-webview-files') {
      // `--min-webview-files` is the retired spelling, still accepted so an
      // old pipeline invocation cannot silently skip the bound.
      options.minMediaFiles = Number(argv[(i += 1)] ?? '');
    } else if (arg === '--ide-load') {
      options.ideLoad = true;
    } else if (!arg.startsWith('--')) {
      options.vsix = resolve(arg);
    }
  }
  return options;
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  if (options.vsix === null || !existsSync(options.vsix)) {
    console.error(
      'usage: node scripts/verify-vsix.mjs <vsix> [--min-media-files N] [--extract-dir <dir>] [--ide-load]',
    );
    return 2;
  }
  if (!Number.isInteger(options.minMediaFiles) || options.minMediaFiles < 1) {
    console.error(`invalid --min-media-files: ${options.minMediaFiles}`);
    return 2;
  }
  console.log(`[verify-vsix] ${options.vsix}`);
  archiveChecks(options.vsix, options.minMediaFiles);
  if (options.extractDir !== null) {
    check(existsSync(options.extractDir), `extracted extension dir exists: ${options.extractDir}`);
    if (existsSync(options.extractDir)) {
      extractedChecks(options.extractDir);
    }
  }
  if (options.ideLoad) {
    ideLoad(options.vsix);
  }
  console.log(`\n[verify-vsix] ${passed} passed, ${failed} failed`);
  return failed > 0 ? 1 : 0;
}

process.exit(main());
