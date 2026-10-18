#!/usr/bin/env node
// Edition 8 real VS Code webview visual matrix.
//
// Renders the 72 Rev8 screens in headless Chrome at:
//   240 / 320 / 480 px x {dark-modern, light-modern, HC dark, HC light}
//   200% text at 320 px for each theme
//   all eight state fixtures at 320 px dark
//
// For every cell it asserts: the screen exists, its full control set renders,
// no control is unnamed or below the 24 px target, no horizontal overflow,
// status is text (not colour-only), focus is visible and high contrast keeps
// component boundaries distinguishable. Structural digests are pinned in a
// platform baseline; screenshots are review artifacts, never the gate.
//
// Usage:
//   node apps/vscode/scripts/edition8-visual.mjs --check
//   node apps/vscode/scripts/edition8-visual.mjs --write
//
// Output: target/certification/edition8/visual-platforms-vscode.raw.json and
// apps/vscode/scripts/baselines/edition8/<platform>.json.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { platform, release, arch } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  E8_FIXTURE_CLASSES,
  E8_SCREENS,
  E8_THEMES,
  renderScreenHtml,
} from './edition8/host.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const BASE = resolve(HERE, 'baselines', 'edition8');
const OUT = resolve(ROOT, 'target/certification/edition8');
const PLATFORM = platform() === 'darwin' ? 'macos' : platform() === 'win32' ? 'windows' : 'linux';
const WRITE = process.argv.includes('--write');
// A baseline written by a machine is not a reviewed baseline until a person
// reviews the captures. CI may execute the matrix and record an unreviewed
// platform boundary; only --reviewed certifies the baseline.
const REVIEWED = WRITE && process.argv.includes('--reviewed');
const WIDTHS = [240, 320, 480];
const THEMES = E8_THEMES.vscode;
const ZOOM_WIDTH = 320;

function sha(bytes) {
  return `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
}

function fontFingerprint() {
  if (platform() === 'darwin') {
    const version = spawnSync('sw_vers', ['-productVersion'], { encoding: 'utf8' }).stdout?.trim() ?? 'unknown';
    return `darwin-${version}-${arch()}-host-ui-font`;
  }
  if (platform() === 'linux') {
    const fonts = spawnSync('fc-list', [':', 'family'], { encoding: 'utf8' }).stdout ?? '';
    return `linux-${release()}-${sha(fonts).slice(0, 20)}`;
  }
  return `windows-${release()}-${arch()}-host-ui-font`;
}

function cells() {
  // A cell is identified by screen/theme/width/zoom/fixture. The state
  // fixtures include `ready`, and the theme matrix already renders
  // dark-modern/320/1/ready; emitting it twice wrote two records for one
  // key (the baseline map silently kept the last) and made the reported
  // cell count disagree with the number of screenshots. Dedupe by key.
  const rows = new Map();
  for (const screen of E8_SCREENS) {
    for (const theme of THEMES) {
      for (const width of WIDTHS) {
        const row = { screen: screen.id, theme, width, zoom: 1, fixture: 'ready' };
        rows.set(`${row.screen}/${row.theme}/${row.width}/${row.zoom}/${row.fixture}`, row);
      }
      const zoomed = { screen: screen.id, theme, width: 320, zoom: 2, fixture: 'ready' };
      rows.set(`${zoomed.screen}/${zoomed.theme}/${zoomed.width}/${zoomed.zoom}/${zoomed.fixture}`, zoomed);
    }
    for (const fixture of E8_FIXTURE_CLASSES) {
      const row = { screen: screen.id, theme: 'dark-modern', width: 320, zoom: 1, fixture };
      rows.set(`${row.screen}/${row.theme}/${row.width}/${row.zoom}/${row.fixture}`, row);
    }
  }
  return [...rows.values()];
}

async function evaluate(cdp, expression) {
  return cdp.evaluate(expression);
}

async function main() {
  const chrome = findChrome();
  if (chrome === null) {
    console.error('edition8 visual: headless Chrome not found');
    process.exit(1);
  }
  const chromeVersion = spawnSync(chrome, ['--version'], { encoding: 'utf8' }).stdout?.trim() ?? 'unknown';
  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  const failures = [];
  const records = [];
  const previousPath = join(BASE, `${PLATFORM}.json`);
  const previous = existsSync(previousPath) ? JSON.parse(readFileSync(previousPath, 'utf8')) : null;
  const previousRecords = new Map((previous?.records ?? []).map((record) => [record.key, record]));
  try {
    for (const cell of cells()) {
      const html = renderScreenHtml({
        screenId: cell.screen,
        fixtureClass: cell.fixture,
        theme: cell.theme,
        width: cell.width,
        zoom: cell.zoom,
      });
      await cdp.send('Emulation.setDeviceMetricsOverride', {
        width: cell.width,
        height: Math.max(720, 320),
        deviceScaleFactor: 1,
        mobile: false,
      });
      await cdp.send('Page.navigate', { url: `data:text/html;charset=utf-8,${encodeURIComponent(html)}` });
      await sleep(45);
      const measure = await evaluate(cdp, `(function () {
        var screen = document.querySelector('[data-screen]');
        var cards = Array.from(document.querySelectorAll('[data-control]'));
        var expected = ${JSON.stringify(E8_SCREENS.find((screen) => screen.id === cell.screen).controls.length)};
        var interactive = Array.from(document.querySelectorAll('button, input, select, textarea, [tabindex="0"]'))
          .filter(function (el) { return el.getClientRects().length > 0; });
        var unnamed = interactive.filter(function (el) {
          if (el.getAttribute('aria-label') || el.getAttribute('aria-labelledby')) return false;
          if (el.id && document.querySelector('label[for="' + el.id + '"]')) return false;
          return !(el.textContent || '').trim();
        }).length;
        var tiny = interactive.filter(function (el) {
          var measured = (el.type === 'checkbox' || el.type === 'radio') ? (el.closest('label') || el) : el;
          var box = measured.getBoundingClientRect();
          return box.width < 24 || box.height < 24;
        }).length;
        var card = document.querySelector('[data-control]');
        if (card) card.focus();
        var focus = card ? getComputedStyle(card) : null;
        var focusReached = card ? document.activeElement === card : false;
        var cardStyle = card ? getComputedStyle(card) : null;
        var bg = getComputedStyle(document.body).backgroundColor;
        var status = document.querySelector('.e8-status');
        var states = cards.map(function (card) { return card.getAttribute('data-state'); });
        return {
          screen: screen ? screen.getAttribute('data-screen') : null,
          fixture: screen ? screen.getAttribute('data-fixture') : null,
          cards: cards.length,
          expected: expected,
          unnamed: unnamed,
          tiny: tiny,
          overflow: document.documentElement.scrollWidth - window.innerWidth,
          status: status ? status.textContent.trim() : null,
          focusOutline: focus ? (focus.outlineStyle + ' ' + focus.outlineWidth) : null,
          focusReached: focusReached,
          border: cardStyle ? cardStyle.borderTopColor : null,
          background: bg,
          states: states.filter(function (value, index, all) { return all.indexOf(value) === index; }).sort(),
          live: document.querySelectorAll('[aria-live]').length,
        };
      })()`);
      if (!measure.screen || measure.screen !== cell.screen) failures.push(`${cell.screen} ${cell.theme} ${cell.width}: screen missing`);
      if (measure.fixture !== cell.fixture) failures.push(`${cell.screen} ${cell.theme} ${cell.width}/${cell.zoom} ${cell.fixture}: fixture not applied`);
      if (measure.cards !== measure.expected) failures.push(`${cell.screen} ${cell.theme} ${cell.width}/${cell.zoom} ${cell.fixture}: ${measure.cards} cards != ${measure.expected}`);
      if (measure.unnamed > 0) failures.push(`${cell.screen} ${cell.theme} ${cell.width}/${cell.zoom} ${cell.fixture}: ${measure.unnamed} unnamed controls`);
      if (measure.tiny > 0) failures.push(`${cell.screen} ${cell.theme} ${cell.width}/${cell.zoom} ${cell.fixture}: ${measure.tiny} sub-24px targets`);
      if (measure.overflow > 1) failures.push(`${cell.screen} ${cell.theme} ${cell.width}/${cell.zoom} ${cell.fixture}: overflow ${measure.overflow}px`);
      if (!measure.status || measure.status.length === 0) failures.push(`${cell.screen}: no status line`);
      if (!measure.focusReached) failures.push(`${cell.screen}: focus was not reached`);
      if (!measure.focusOutline || measure.focusOutline.startsWith('none')) failures.push(`${cell.screen}: focus outline absent`);
      if (measure.live === 0) failures.push(`${cell.screen}: no live status region`);
      if (cell.fixture === 'empty' && measure.states.some((state) => state === 'available')) failures.push(`${cell.screen}: empty fixture claims an available control`);
      if (cell.fixture === 'stale' && !measure.states.includes('stale') && !measure.states.includes('disabled')) failures.push(`${cell.screen}: stale fixture is not marked stale/disabled`);
      if (cell.fixture === 'error' && !measure.states.includes('error') && !measure.states.includes('disabled')) failures.push(`${cell.screen}: error fixture is not marked error/disabled`);
      if (cell.theme.startsWith('high-contrast') && measure.border === measure.background) failures.push(`${cell.screen}: HC border equals background`);
      const key = `${cell.screen}/${cell.theme}/${cell.width}/${cell.zoom}/${cell.fixture}`;
      const digest = sha(JSON.stringify({ key, measure }));
      const prior = previousRecords.get(key);
      if (!WRITE && prior && prior.digest !== digest) failures.push(`${key}: structural digest drifted`);
      if (!WRITE && !prior) failures.push(`${key}: no reviewed baseline record for ${PLATFORM}`);
      let screenshot = null;
      if (WRITE) {
        const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
        const bytes = Buffer.from(shot.data, 'base64');
        const path = resolve(OUT, 'visual/vscode', PLATFORM, cell.screen, cell.theme, String(cell.width), String(cell.zoom), `${cell.fixture}.png`);
        mkdirSync(dirname(path), { recursive: true });
        writeFileSync(path, bytes);
        screenshot = sha(bytes);
      }
      records.push({ key, screen: cell.screen, theme: cell.theme, width: cell.width, zoom: cell.zoom, fixture: cell.fixture, digest, measure, screenshot });
    }
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }
  const manifest = {
    schema: 'faktor-edition8-vscode-visual/v1',
    platform: PLATFORM,
    host: 'vscode',
    chrome: chromeVersion,
    os: `${platform()} ${release()} ${arch()}`,
    font_fingerprint: fontFingerprint(),
    reviewed: REVIEWED,
    generated_at: new Date().toISOString(),
    cells: records.length,
    screens: E8_SCREENS.length,
    records,
  };
  if (WRITE && failures.length === 0) {
    mkdirSync(BASE, { recursive: true });
    writeFileSync(previousPath, `${JSON.stringify(manifest, null, 2)}\n`);
    console.log(`edition8 visual vscode: wrote ${records.length} ${REVIEWED ? 'reviewed' : 'unreviewed'} ${PLATFORM} records to ${previousPath}`);
  }
  const raw = {
    schema: 'faktor-edition8-visual-host/v1',
    host: 'vscode',
    platform: PLATFORM,
    generated_at: new Date().toISOString(),
    manifest: WRITE ? manifest : (previous ?? null),
    checks: records.map((record) => ({ key: record.key, digest: record.digest, measure: record.measure })),
    counts: {
      screens: E8_SCREENS.length,
      cells: records.length,
      failed: failures.length,
      themes: THEMES.length,
      widths: WIDTHS.length,
      fixture_classes: E8_FIXTURE_CLASSES.length,
    },
    failures,
  };
  // Bind the certificate to the exact records it consumed. A per-platform
  // copy is written as well, so a macOS run cannot overwrite the Linux
  // evidence another lane produced.
  raw.sha256 = sha(JSON.stringify({ platform: PLATFORM, manifest: raw.manifest, checks: raw.checks }));
  mkdirSync(OUT, { recursive: true });
  const payload = `${JSON.stringify(raw, null, 2)}\n`;
  writeFileSync(resolve(OUT, `visual-platforms-vscode.${PLATFORM}.raw.json`), payload);
  writeFileSync(resolve(OUT, 'visual-platforms-vscode.raw.json'), payload);
  for (const failure of failures.slice(0, 50)) console.error(`FAIL ${failure}`);
  console.log(
    `edition8 visual vscode: ${records.length} cells (${E8_SCREENS.length} screens x ${THEMES.length} themes x ${WIDTHS.length} widths + 200% + 8 states); ${failures.length} failures ` +
      `(${WRITE ? 'wrote' : 'checked'} ${PLATFORM})`,
  );
  if (failures.length > 0) process.exit(1);
}

await main();
