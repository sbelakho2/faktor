#!/usr/bin/env node
// Edition 8 visual/accessibility/perf matrix in real headless Chrome.
//
//   --check   default: semantic + optical + contrast checks and the
//             240/320/480/720 matrix; writes no PNG baselines.
//   --write   additionally writes the screenshot matrix:
//             base dark 240/320/480/720 (288), HC 240/320 (144),
//             200% text 480 (72), light 480 (72) = 576 PNGs.
//
// The report is honest per platform: a missing Chrome binary reports
// not_certified, never a pass.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir, platform } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { E8_SCREENS, E8_CONTROLS } from '../src/generated/edition8Registry.ts';
import { renderEdition8Html } from '../src/edition8/html.ts';
import { destinationOf } from '../src/edition8/view.ts';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';
import { edition8Snapshot, edition8EmptySnapshot, edition8StaleSnapshot } from './fixtures/edition8.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const MEDIA = join(HERE, '..', 'media');
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const PLATFORM = platform();
const WRITE = process.argv.includes('--write');
const WIDTHS = [240, 320, 480, 720];
const THEMES = {
  dark: `
    :root { color-scheme: dark; --vscode-editor-background:#1e1e1e; --vscode-foreground:#d4d4d4; --vscode-sideBar-background:#252526; --vscode-panel-border:#3c3c3c; --vscode-widget-border:#5a5a5a; --vscode-descriptionForeground:#a7a7a7; --vscode-button-background:#0e639c; --vscode-button-foreground:#ffffff; --vscode-button-hoverBackground:#1177bb; --vscode-button-secondaryBackground:#3a3d41; --vscode-button-secondaryForeground:#ffffff; --vscode-input-background:#3c3c3c; --vscode-input-foreground:#cccccc; --vscode-input-border:#5a5a5a; --vscode-focusBorder:#007fd4; --vscode-editorWarning-foreground:#cca700; --vscode-errorForeground:#f14c4c; --vscode-testing-iconPassed:#73c991; --vscode-list-activeSelectionBackground:#094771; --vscode-editorWidget-background:#252526; --vscode-textCodeBlock-background:#2d2d2d; --vscode-charts-blue:#3794ff; }
    body { background:#1e1e1e; color:#d4d4d4; }
  `,
  light: `
    :root { color-scheme: light; --vscode-editor-background:#ffffff; --vscode-foreground:#3b3b3b; --vscode-sideBar-background:#f3f3f3; --vscode-panel-border:#d4d4d4; --vscode-widget-border:#b0b0b0; --vscode-descriptionForeground:#6f6f6f; --vscode-button-background:#007acc; --vscode-button-foreground:#ffffff; --vscode-button-hoverBackground:#0062a3; --vscode-button-secondaryBackground:#e8e8e8; --vscode-button-secondaryForeground:#3b3b3b; --vscode-input-background:#ffffff; --vscode-input-foreground:#3b3b3b; --vscode-input-border:#cecece; --vscode-focusBorder:#0090f1; --vscode-editorWarning-foreground:#bf8803; --vscode-errorForeground:#a1260d; --vscode-testing-iconPassed:#388a34; --vscode-list-activeSelectionBackground:#e4f1fb; --vscode-editorWidget-background:#f8f8f8; --vscode-textCodeBlock-background:#f3f3f3; --vscode-charts-blue:#0066bf; }
    body { background:#ffffff; color:#3b3b3b; }
  `,
  hc: `
    :root { color-scheme: dark; --vscode-editor-background:#000000; --vscode-foreground:#ffffff; --vscode-sideBar-background:#000000; --vscode-panel-border:#ffffff; --vscode-widget-border:#ffffff; --vscode-descriptionForeground:#ffffff; --vscode-button-background:#000000; --vscode-button-foreground:#ffffff; --vscode-button-hoverBackground:#1a1a1a; --vscode-button-secondaryBackground:#000000; --vscode-button-secondaryForeground:#ffffff; --vscode-input-background:#000000; --vscode-input-foreground:#ffffff; --vscode-input-border:#ffffff; --vscode-focusBorder:#ffd700; --vscode-editorWarning-foreground:#ffd700; --vscode-errorForeground:#ff6b6b; --vscode-testing-iconPassed:#7cff7c; --vscode-list-activeSelectionBackground:#1a1a1a; --vscode-editorWidget-background:#000000; --vscode-textCodeBlock-background:#000000; --vscode-charts-blue:#7ad7ff; }
    body { background:#000000; color:#ffffff; }
    .e8-primary, .e8-secondary, input, select, textarea, button { border-width:2px; }
  `,
};

function esc(value) {
  return String(value).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function hash(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function modeFor(screen) {
  return destinationOf(screen) === 'settings' ? 'settings' : 'shell';
}

function pageHtml(screen, snapshot, theme, textScale = 1) {
  let html = renderEdition8Html({
    nonce: 'N',
    cspSource: '*',
    scriptUri: pathToFileURL(join(temp, 'edition8.js')).href,
    styleUri: pathToFileURL(join(temp, 'edition8.css')).href,
    state: { snapshot, loading: false, error: null, focusControl: null },
    initialScreen: screen.id,
    host: 'vscode',
    mode: modeFor(screen),
  });
  html = html.replace(
    '</head>',
    `<style nonce="N">${THEMES[theme]}${textScale !== 1 ? `html{font-size:${textScale * 100}%;}` : ''}</style></head>`,
  );
  html = html.replace(
    '<script nonce="N">window.__faktorE8Initial',
    `<script nonce="N">window.__posted=[];window.__ready=true;window.acquireVsCodeApi=function(){return{postMessage:function(m){window.__posted.push(m);}};};</script><script nonce="N">window.__faktorE8Initial`,
  );
  return html;
}

const temp = mkdtempSync(join(tmpdir(), 'faktor-e8-render-'));
writeFileSync(join(temp, 'edition8.js'), readFileSync(join(MEDIA, 'edition8.js')));
writeFileSync(join(temp, 'edition8.css'), readFileSync(join(MEDIA, 'edition8.css')));

const rich = edition8Snapshot();
const empty = edition8EmptySnapshot();
const stale = edition8StaleSnapshot();

const VARIANTS = new Map();
function variantFile(screen, snapshot, theme, textScale) {
  const key = `${screen.id}:${theme}:${textScale}:${snapshot === rich ? 'rich' : snapshot === empty ? 'empty' : 'stale'}`;
  if (!VARIANTS.has(key)) {
    const file = join(temp, `${key.replace(/[^a-z0-9]+/gi, '-')}.html`);
    writeFileSync(file, pageHtml(screen, snapshot, theme, textScale));
    VARIANTS.set(key, file);
  }
  return VARIANTS.get(key);
}

const MEASURE = String.raw`(() => {
  const screenId = window.__measureScreen;
  const screen = document.querySelector('[data-screen="' + screenId + '"]');
  if (!screen || screen.hidden) return { ok: false, reason: 'screen not visible' };
  const cards = Array.from(screen.querySelectorAll('[data-control]'));
  const visible = (el) => el.getClientRects().length > 0;
  const interactive = Array.from(screen.querySelectorAll('button, input, select, textarea, summary')).filter(visible);
  const tiny = interactive.filter((el) => {
    const measured = (el.type === 'checkbox' || el.type === 'radio') ? (el.closest('label') || el) : el;
    const r = measured.getBoundingClientRect();
    return r.width < 24 || r.height < 24;
  }).map((el) => {
    const r = el.getBoundingClientRect();
    return (el.tagName + (el.id ? '#' + el.id : '') + ' ' + Math.round(r.width) + 'x' + Math.round(r.height)).slice(0, 80);
  });
  const unnamed = interactive.filter((el) => {
    if (el.getAttribute('aria-label')) return false;
    if (['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName)) {
      return !(el.closest('label') || (el.id && document.querySelector('label[for="' + el.id + '"]')));
    }
    return !((el.textContent || '').trim().length > 0);
  });
  const modeOf = (el) => {
    const c = getComputedStyle(el);
    return { color: c.color, background: c.backgroundColor, size: parseFloat(c.fontSize), weight: parseInt(c.fontWeight, 10) || 400 };
  };
  const parse = (value) => {
    const m = /rgba?\(([^)]+)\)/.exec(value || '');
    if (!m) return null;
    const parts = m[1].split(',').map((entry) => parseFloat(entry.trim()));
    return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
  };
  const lum = (c) => {
    const ch = (v) => { v /= 255; return v <= 0.03928 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4); };
    return 0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b);
  };
  const ratio = (a, b) => {
    const l1 = lum(a), l2 = lum(b);
    return (Math.max(l1, l2) + 0.05) / (Math.min(l1, l2) + 0.05);
  };
  const effectiveBg = (el) => {
    let node = el;
    while (node && node !== document.documentElement) {
      const c = parse(getComputedStyle(node).backgroundColor);
      if (c && c.a > 0.95) return c;
      node = node.parentElement;
    }
    return parse(getComputedStyle(document.body).backgroundColor) || { r: 30, g: 30, b: 30, a: 1 };
  };
  const textNodes = Array.from(screen.querySelectorAll('h2, h3, p, dt, dd, .e8-row-value, .e8-help, .e8-control-state, .e8-state-badge, .e8-status, button, .e8-tab')).filter(visible);
  const contrastFailures = [];
  let minContrast = 99;
  for (const el of textNodes) {
    const fg = parse(getComputedStyle(el).color);
    if (!fg) continue;
    const bg = effectiveBg(el);
    const r = ratio(fg, bg);
    if (r < minContrast) minContrast = r;
    const meta = modeOf(el);
    const large = meta.size >= 18.66 || (meta.size >= 14 && meta.weight >= 700);
    const required = large ? 3 : 4.5;
    if (r + 0.01 < required) {
      contrastFailures.push({ text: (el.textContent || '').trim().slice(0, 60), ratio: Math.round(r * 100) / 100, required });
    }
  }
  const optical = [];
  for (const button of Array.from(screen.querySelectorAll('.e8-primary')).filter(visible)) {
    const text = (button.textContent || '').trim();
    if (!text) continue;
    const range = document.createRange();
    range.selectNodeContents(button);
    const tr = range.getBoundingClientRect();
    const br = button.getBoundingClientRect();
    const cs = getComputedStyle(button);
    const innerLeft = br.left + parseFloat(cs.paddingLeft);
    const innerRight = br.right - parseFloat(cs.paddingRight);
    const innerTop = br.top + parseFloat(cs.paddingTop);
    const innerBottom = br.bottom - parseFloat(cs.paddingBottom);
    const dx = Math.abs((tr.left + tr.width / 2) - (innerLeft + innerRight) / 2);
    const dy = Math.abs((tr.top + tr.height / 2) - (innerTop + innerBottom) / 2);
    optical.push({ text, dx: Math.round(dx * 100) / 100, dy: Math.round(dy * 100) / 100 });
  }
  const states = cards.map((card) => card.getAttribute('data-state'));
  return {
    ok: true,
    expected: ${0},
    cards: cards.length,
    visible_cards: cards.filter(visible).length,
    overflow: Math.max(0, document.documentElement.scrollWidth - window.innerWidth),
    tiny: tiny.length,
    tiny_labels: tiny.slice(0, 10),
    unnamed: unnamed.length,
    statuses: Array.from(new Set(states)),
    min_contrast: Math.round(minContrast * 100) / 100,
    contrast_failures: contrastFailures.slice(0, 8),
    optical,
    optical_max_dx: optical.reduce((max, row) => Math.max(max, row.dx), 0),
    optical_max_dy: optical.reduce((max, row) => Math.max(max, row.dy), 0),
    rows: screen.querySelectorAll('[data-row-key]').length,
    status_line: screen.querySelectorAll('.e8-status').length,
    live_regions: screen.querySelectorAll('[aria-live]').length,
  };
})()`;

async function navigate(cdp, file, width, theme) {
  await cdp.send('Emulation.setDeviceMetricsOverride', { width, height: 900, deviceScaleFactor: 1, mobile: false });
  if (theme === 'hc') {
    await cdp.send('Emulation.setEmulatedMedia', {
      features: [
        { name: 'forced-colors', value: 'active' },
        { name: 'prefers-contrast', value: 'more' },
      ],
    });
  } else {
    await cdp.send('Emulation.setEmulatedMedia', { features: [] });
  }
  const loaded = cdp.waitFor('Page.loadEventFired');
  await cdp.send('Page.navigate', { url: pathToFileURL(file).href });
  await loaded;
  await sleep(80);
  await cdp.evaluate('new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))');
}

async function measureScreen(cdp, screen) {
  await cdp.evaluate(`(() => {
    window.__measureScreen = ${JSON.stringify(screen.id)};
    var link = document.querySelector('[data-screen-link="${screen.id}"]');
    if (link) link.click();
  })()`);
  await sleep(30);
  return cdp.evaluate(MEASURE.replace('${0}', String(screen.controls.length)));
}

async function capture(cdp, file) {
  const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
  const bytes = Buffer.from(shot.data, 'base64');
  if (bytes.length === 0) throw new Error(`empty screenshot ${file}`);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, bytes);
  return { bytes: bytes.length, hash: hash(bytes) };
}

async function main() {
  const chrome = findChrome();
  const generatedAt = new Date().toISOString();
  if (chrome === null) {
    const document = { schema: 'faktor-edition8-visual/v1', platform: PLATFORM, status: 'not_certified', reason: 'headless Chrome not found', generated_at: generatedAt };
    mkdirSync(OUT, { recursive: true });
    writeFileSync(resolve(OUT, 'visual-matrix-vscode.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
    console.error('edition8 visual: headless Chrome not found (not_certified)');
    process.exitCode = 1;
    return;
  }
  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  const failures = [];
  const matrix = [];
  const screenshots = [];
  const baseDir = resolve(OUT, 'screens', PLATFORM);
  const byMode = { shell: [], settings: [] };
  for (const screen of E8_SCREENS) byMode[modeFor(screen)].push(screen);

  async function runPage(snapshot, theme, textScale, width, screens, label, captureShots) {
    console.log(`visual: ${label} ${width}px (${screens.length} screens)`);
    const first = screens[0];
    await navigate(cdp, variantFile(first, snapshot, theme, textScale), width, theme);
    for (const screen of screens) {
      const result = await measureScreen(cdp, screen);
      if (!result.ok) {
        failures.push(`${screen.id} ${label}: ${result.reason}`);
        continue;
      }
      if (result.cards !== screen.controls.length) failures.push(`${screen.id} ${label}: ${result.cards}/${screen.controls.length} cards`);
      if (result.overflow > 1) failures.push(`${screen.id} ${label}: horizontal overflow ${result.overflow}px`);
      if (result.tiny > 0) failures.push(`${screen.id} ${label}: ${result.tiny} targets under 24px`);
      if (result.unnamed > 0) failures.push(`${screen.id} ${label}: ${result.unnamed} unnamed controls`);
      if (result.min_contrast < 4.5) failures.push(`${screen.id} ${label}: contrast ${result.min_contrast} (${JSON.stringify(result.contrast_failures.slice(0, 2))})`);
      if (result.optical_max_dx > 1.5 || result.optical_max_dy > 1.5) failures.push(`${screen.id} ${label}: optical dx=${result.optical_max_dx} dy=${result.optical_max_dy}`);
      matrix.push({ screen: screen.id, width, theme, text_scale: textScale, fixture: label, ...result, expected: screen.controls.length });
      if (captureShots) {
        const dir = label === 'hc' ? 'hc' : label === '200%' ? 'text200' : label === 'light' ? 'light' : 'dark';
        screenshots.push({ screen: screen.id, width, theme, text_scale: textScale, label, ...(await capture(cdp, join(baseDir, dir, `${screen.id}-${width}.png`))) });
      }
    }
  }

  try {
    for (const width of WIDTHS) {
      for (const mode of ['shell', 'settings']) {
        await runPage(rich, 'dark', 1, width, byMode[mode], 'dark', WRITE);
      }
    }
    if (WRITE) {
      for (const width of [240, 320]) {
        for (const mode of ['shell', 'settings']) {
          await runPage(rich, 'hc', 1, width, byMode[mode], 'hc', true);
        }
      }
      for (const mode of ['shell', 'settings']) {
        await runPage(rich, 'dark', 2, 480, byMode[mode], '200%', true);
        await runPage(rich, 'light', 1, 480, byMode[mode], 'light', true);
      }
    }
    const SAMPLE = new Set(['S01', 'S02', 'S08', 'S14', 'S24', 'S30', 'S41', 'S45', 'S53', 'S60', 'S71', 'S72']);
    for (const [name, snapshot] of [['empty', empty], ['stale', stale]]) {
      for (const width of [240, 320, 480]) {
        for (const mode of ['shell', 'settings']) {
          console.log(`visual: ${name} ${width}px ${mode}`);
          const screens = byMode[mode].filter((screen) => SAMPLE.has(screen.id));
          if (screens.length === 0) continue;
          const first = screens[0];
          await navigate(cdp, variantFile(first, snapshot, 'dark', 1), width, 'dark');
          for (const screen of screens) {
            const result = await measureScreen(cdp, screen);
            if (!result.ok) {
              failures.push(`${screen.id} ${name} ${width}: ${result.reason}`);
              continue;
            }
            if (result.overflow > 1) failures.push(`${screen.id} ${name} ${width}: overflow ${result.overflow}`);
            if (name === 'empty' && result.statuses.some((state) => state === 'available')) failures.push(`${screen.id} empty ${width}: available state without served data`);
            if (name === 'stale' && result.statuses.some((state) => state === 'available')) failures.push(`${screen.id} stale ${width}: current state claimed`);
            matrix.push({ screen: screen.id, width, theme: 'dark', fixture: name, ...result });
          }
        }
      }
    }
  } finally {
    try { child.kill('SIGKILL'); } catch { /* ignore */ }
  }

  const contrastRatios = matrix.filter((row) => typeof row.min_contrast === 'number').map((row) => row.min_contrast);
  const opticalDx = matrix.flatMap((row) => (row.optical ?? []).map((entry) => entry.dx));
  const opticalDy = matrix.flatMap((row) => (row.optical ?? []).map((entry) => entry.dy));
  const document = {
    schema: 'faktor-edition8-visual/v1',
    platform: PLATFORM,
    host: 'vscode',
    status: failures.length === 0 ? 'pass' : 'fail',
    generated_at: generatedAt,
    chrome: spawnSync(chrome, ['--version'], { encoding: 'utf8' }).stdout?.trim() || 'unknown',
    counts: {
      screens: E8_SCREENS.length,
      controls: E8_CONTROLS.length,
      matrix_measurements: matrix.length,
      base_widths: WIDTHS,
      screenshots: screenshots.length,
      dark_screens: matrix.filter((row) => row.theme === 'dark' && row.text_scale === 1 && row.fixture === 'dark').length,
      hc_screens: matrix.filter((row) => row.fixture === 'hc').length,
      text200_screens: matrix.filter((row) => row.fixture === '200%').length,
      light_screens: matrix.filter((row) => row.fixture === 'light').length,
      empty_screens: matrix.filter((row) => row.fixture === 'empty').length,
      stale_screens: matrix.filter((row) => row.fixture === 'stale').length,
    },
    optical: {
      samples: opticalDx.length,
      max_dx: Math.round(Math.max(0, ...opticalDx) * 100) / 100,
      max_dy: Math.round(Math.max(0, ...opticalDy) * 100) / 100,
      threshold: 1.5,
    },
    contrast: {
      samples: contrastRatios.length,
      min: Math.round(Math.min(...contrastRatios) * 100) / 100,
      threshold: 4.5,
    },
    failures,
    matrix,
    screenshots,
  };
  mkdirSync(OUT, { recursive: true });
  writeFileSync(resolve(OUT, 'visual-matrix-vscode.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
  console.log(
    `vscode edition8 visual: ${document.status} · ${matrix.length} measurements, ${screenshots.length} screenshots · ` +
      `contrast min ${document.contrast.min} (${contrastRatios.length}), optical dx ${document.optical.max_dx}/dy ${document.optical.max_dy} (${opticalDx.length}) · failures ${failures.length}`,
  );
  for (const failure of failures.slice(0, 25)) console.error(`FAIL ${failure}`);
  child.child.kill('SIGKILL');
  if (failures.length > 0) process.exit(1);
}

await main();
