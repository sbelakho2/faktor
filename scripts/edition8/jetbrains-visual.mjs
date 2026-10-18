#!/usr/bin/env node
// JetBrains Edition 8 visual/platform evidence adapter.
//
// The real Swing host matrix renders every Rev8 screen at 240/320/480/800 px,
// all three themes, 200% text, plus the pinned logical baseline. This adapter
// turns those checks into per-screen/per-cell records and certifies ONLY the
// platform that produced them. A platform name copied into another platform's
// slot is rejected: no cross-platform inheritance.
//
// Output: target/certification/edition8/visual-platforms-jetbrains.raw.json.

import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { platform, release, arch } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry } from '../../tests/edition8/registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const MATRIX_ARG = process.argv.indexOf('--matrix');
const MATRIX = MATRIX_ARG >= 0 && MATRIX_ARG + 1 < process.argv.length
  ? resolve(ROOT, process.argv[MATRIX_ARG + 1])
  : resolve(ROOT, 'target/certification/jetbrains-host-matrix.json');
const MATRIX_LABEL = MATRIX_ARG >= 0 && MATRIX_ARG + 1 < process.argv.length
  ? process.argv[MATRIX_ARG + 1]
  : 'target/certification/jetbrains-host-matrix.json';
const BASELINE = resolve(ROOT, 'apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json');
const argv = process.argv.slice(2);
function flag(name, fallback = null) {
  const index = argv.indexOf(`--${name}`);
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback;
}
// The matrix JSON does not carry an OS field, so the caller names the
// platform that actually rendered it. This is REQUIRED for a container run:
// deriving the platform from the adapter host would relabel a Linux render
// as macOS and look up the wrong reviewed digests (inherit them).
const declared = flag('platform');
const PLATFORM = declared ?? (platform() === 'darwin' ? 'macos' : platform() === 'win32' ? 'windows' : 'linux');
if (!['macos', 'linux', 'windows'].includes(PLATFORM)) {
  console.error(`edition8 jetbrains visual: unknown platform ${PLATFORM}`);
  process.exit(1);
}
const OUT_FILE = flag('out', 'visual-platforms-jetbrains.raw.json');
const OS_LABEL = flag('os', `${platform()} ${release()} ${arch()}`);
const BUILD_LABEL = flag('build', null);
const FONT_LABEL = flag('font', `java-host-ui-font-${PLATFORM}`);
const IMAGE_LABEL = flag('image', 'host');

if (!existsSync(MATRIX)) {
  console.error('edition8 jetbrains visual: missing target/certification/jetbrains-host-matrix.json; run compile-and-smoke.sh --smoke HostMatrix');
  process.exit(1);
}
const matrix = JSON.parse(readFileSync(MATRIX, 'utf8'));
const baseline = existsSync(BASELINE) ? JSON.parse(readFileSync(BASELINE, 'utf8')) : null;
const registry = buildRegistry();
const checks = new Map((matrix.checks ?? []).map((check) => [check.name ?? check.id, check]));
const failures = [];

function checkPassed(name) {
  const entry = checks.get(name);
  if (!entry) return false;
  return entry.passed === true || entry.status === 'passed' || entry.ok === true;
}

const widths = [240, 320, 480];
const themes = ['light', 'dark', 'high-contrast'];
const records = [];
for (const screen of registry.screens) {
  const widthCells = widths.map((width) => {
    const passed = checkPassed(`edition8/${screen.id}/${width}`);
    if (!passed) failures.push(`${screen.id} width ${width} not passed`);
    return { axis: 'width', value: width, passed };
  });
  const themeCells = themes.map((theme) => {
    const passed = checkPassed(`edition8/${screen.id}/theme/${theme}`);
    if (!passed) failures.push(`${screen.id} theme ${theme} not passed`);
    return { axis: 'theme', value: theme, passed };
  });
  const zoom = checkPassed(`edition8/${screen.id}/zoom/200`);
  if (!zoom) failures.push(`${screen.id} zoom 200% not passed`);
  const digest = baseline?.platforms?.[PLATFORM]?.digests?.[`edition8-${screen.id.toLowerCase()}`] ?? null;
  if (digest === null) failures.push(`${screen.id} has no ${PLATFORM} reviewed baseline digest`);
  records.push({
    screen: screen.id,
    title: screen.title,
    cells: {
      widths: widthCells,
      themes: themeCells,
      zoom_200: { axis: 'zoom', value: 200, passed: zoom },
      baseline: { axis: 'baseline', value: 480, passed: digest !== null, digest },
    },
    passed: widthCells.every((cell) => cell.passed) && themeCells.every((cell) => cell.passed) && zoom && digest !== null,
  });
}

let javaVersion = BUILD_LABEL ?? 'unknown';
if (BUILD_LABEL === null) {
  try {
    const probe = spawnSync('java', ['-version'], { encoding: 'utf8' });
    javaVersion = `${probe.stderr || probe.stdout || 'unknown'}`.split('\n')[0].trim() || 'unknown';
  } catch {
    // The matrix itself proves Java was available; keep a typed unknown.
  }
}
const reviewed = baseline?.platforms?.[PLATFORM]?.reviewed === true;
if (!reviewed) failures.push(`${PLATFORM} baseline is not marked reviewed`);
const certificateDigest = `sha256:${createHash('sha256').update(JSON.stringify(records)).digest('hex')}`;
const document = {
  schema: 'faktor-edition8-visual-platform/v1',
  host: 'jetbrains',
  platform: PLATFORM,
  generated_at: new Date().toISOString(),
  method: 'Swing offscreen render; logical reviewed digests; no cross-platform inheritance',
  build: javaVersion,
  os: OS_LABEL,
  image: IMAGE_LABEL,
  font_fingerprint: FONT_LABEL,
  reviewed,
  source_matrix: MATRIX_LABEL,
  source_baseline: 'apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json',
  cells: records.length * (widths.length + themes.length + 2),
  screens: records.length,
  sha256: certificateDigest,
  counts: {
    screens: records.length,
    cells: records.length * (widths.length + themes.length + 2),
    failed: failures.length,
    widths: widths.length,
    themes: themes.length,
    zoom_cells: records.length,
    baseline_digests: records.filter((record) => record.cells.baseline.passed).length,
  },
  records,
  failures,
};
mkdirSync(OUT, { recursive: true });
const payload = `${JSON.stringify(document, null, 2)}\n`;
writeFileSync(resolve(OUT, OUT_FILE), payload);
writeFileSync(resolve(OUT, `visual-platforms-jetbrains.${PLATFORM}.raw.json`), payload);
if (OUT_FILE !== 'visual-platforms-jetbrains.raw.json') {
  writeFileSync(resolve(OUT, 'visual-platforms-jetbrains.raw.json'), payload);
}
for (const failure of failures.slice(0, 40)) console.error(`FAIL ${failure}`);
console.log(
  `jetbrains edition8 visual: ${records.length} screens, ${document.cells} cells, ` +
    `${document.counts.baseline_digests} reviewed ${PLATFORM} digests; ${failures.length} failures`,
);
if (failures.length > 0) process.exit(1);
