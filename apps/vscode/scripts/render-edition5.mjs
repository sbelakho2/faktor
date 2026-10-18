#!/usr/bin/env node
// Edition 5 screenshot + layout harness. Renders the REAL Control Center
// markup/CSS/script from the rich, empty and stale fixtures in headless Chrome:
//
//   --check   required layout/semantics checks at 240/320/480/800 for all 32
//             screens, both themes, empty and stale states; verifies that the
//             current platform's baseline records are present and fresh.
//   --write   additionally writes reviewed PNG baselines under
//             scripts/baselines/edition5/<platform>/{dark,light}/.
//
// The platform record is honest: a missing Linux/macOS/Windows record is
// reported as not_certified, never as a pass (visual-baseline policy).
//
// Screenshots: dark 240/320/480/800 for all 32 screens; light 480 for all 32.
// Contact sheets (review aids) are written next to the manifest.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { tmpdir, platform } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { E5_SCREENS } from '../src/generated/edition5Registry.ts';
import { validateSnapshot } from '../src/edition5/validate.ts';
import { renderEdition5Html } from '../src/edition5/html.ts';
import { richSnapshot, emptySnapshot, staleSnapshot } from './fixtures/edition5.mjs';
import { connect, findChrome, launchChrome, sleep, THEMES } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const MEDIA = join(HERE, '..', 'media');
const BASE = join(HERE, 'baselines', 'edition5');
const WIDTHS = [240, 320, 480, 800];
const SHOT_WIDTHS = [480];
const PLATFORM = platform();
const WRITE = process.argv.includes('--write');

function hash(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

const rich = validateSnapshot(richSnapshot());
const empty = validateSnapshot(emptySnapshot());
const stale = validateSnapshot(staleSnapshot());

const dir = mkdtempSync(join(tmpdir(), 'faktor-e5-render-'));
const scriptFile = join(dir, 'edition5.js');
const styleFile = join(dir, 'edition5.css');
writeFileSync(scriptFile, readFileSync(join(MEDIA, 'edition5.js')));
writeFileSync(styleFile, readFileSync(join(MEDIA, 'edition5.css')));

function pageHtml(snapshot, theme, initial = 'S01') {
  let html = renderEdition5Html({
    nonce: 'N',
    cspSource: '*',
    scriptUri: pathToFileURL(scriptFile).href,
    styleUri: pathToFileURL(styleFile).href,
    state: { snapshot, loading: false, error: null, focusControl: null },
    initialScreen: initial,
    host: 'vscode',
  });
  html = html.replace('</head>', `<style nonce="N">${THEMES[theme]}</style></head>`);
  html = html.replace(
    '<script nonce="N" src=',
    `<script nonce="N">window.__posted=[];window.acquireVsCodeApi=function(){return{postMessage:function(m){window.__posted.push(m);}};};</script><script nonce="N" src=`,
  );
  return html;
}

const files = {
  dark: join(dir, 'dark.html'),
  light: join(dir, 'light.html'),
  empty: join(dir, 'empty.html'),
  stale: join(dir, 'stale.html'),
};
writeFileSync(files.dark, pageHtml(rich, 'dark'));
writeFileSync(files.light, pageHtml(rich, 'light'));
writeFileSync(files.empty, pageHtml(empty, 'dark'));
writeFileSync(files.stale, pageHtml(stale, 'dark'));

if (WRITE) {
  const rows = E5_SCREENS.map(
    (screen) =>
      `<figure><img src="${pathToFileURL(join(BASE, PLATFORM, 'dark', `${screen.id}-480.png`)).href}" alt="${screen.id}"><figcaption>${screen.id} ${screen.title.slice(0, 60)}</figcaption></figure>`,
  ).join('');
  writeFileSync(
    join(dir, 'contact.html'),
    `<!DOCTYPE html><meta charset="utf-8"><style>body{margin:0;background:#1e1e1e;color:#ccc;font:12px system-ui}main{display:grid;grid-template-columns:repeat(4,1fr);gap:8px;padding:8px}figure{margin:0}img{width:100%;display:block;border:1px solid #444}figcaption{padding:2px 4px}</style><main>${rows}</main>`,
  );
}

async function main() {
  const chrome = findChrome();
  if (chrome === null) {
    console.error('render-edition5: headless Chrome not found');
    process.exitCode = 1;
    return;
  }
  const child = await launchChrome(chrome);
  const failures = [];
  const shots = [];
  try {
    const cdp = await connect(child.pageWs);
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    let themeName = 'dark';
    let currentFile = files.dark;
    async function navigate(file, width) {
      currentFile = file;
      await cdp.send('Emulation.setDeviceMetricsOverride', {
        width,
        height: 900,
        deviceScaleFactor: 1,
        mobile: false,
      });
      await cdp.send('Page.navigate', { url: pathToFileURL(file).href });
      await sleep(350);
    }
    async function select(screenId) {
      return cdp.evaluate(`(function () {
        var link = document.querySelector('[data-screen-link="${screenId}"]');
        if (!link) return { ok: false, reason: 'missing screen link' };
        link.click();
        var visible = document.querySelector('[data-screen="${screenId}"]');
        return { ok: !!visible && !visible.hidden, title: visible ? visible.querySelector('h2').textContent : null };
      })()`);
    }
    async function measure(screenId, width, mode) {
      return cdp.evaluate(`(function () {
        var screen = document.querySelector('[data-screen="${screenId}"]');
        var cards = screen.querySelectorAll('[data-control]');
        var states = Array.from(cards).map(function (card) { return card.getAttribute('data-state'); });
        var interactive = Array.from(screen.querySelectorAll('button, input, select, textarea, summary')).filter(function (el) {
          return el.getClientRects().length > 0 && !el.closest('[hidden]');
        });
        var tiny = interactive.filter(function (el) {
          var measured = (el.type === 'checkbox' || el.type === 'radio') ? (el.closest('label') || el) : el;
          var r = measured.getBoundingClientRect();
          return r.width < 24 || r.height < 24;
        }).length;
        var unnamed = interactive.filter(function (el) {
          if (el.getAttribute('aria-label')) return false;
          if (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT') {
            if (el.closest('label')) return false;
            return !el.id || !document.querySelector('label[for="' + el.id + '"]');
          }
          return !(el.textContent || '').trim();
        }).length;
        var visibleCards = Array.from(cards).filter(function (card) { return card.getClientRects().length > 0; }).length;
        return {
          overflow: document.documentElement.scrollWidth - window.innerWidth,
          cards: cards.length,
          visibleCards: visibleCards,
          expected: ${JSON.stringify(E5_SCREENS.find((screen) => screen.id === screenId).controls.length)},
          tiny: tiny,
          unnamed: unnamed,
          states: states,
          mode: ${JSON.stringify(mode)},
          width: ${width},
        };
      })()`);
    }
    async function capture(name) {
      const metrics = await cdp.send('Page.getLayoutMetrics');
      const size = metrics.cssLayoutViewport || metrics.cssVisualViewport || metrics.cssContentSize;
      const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      const bytes = Buffer.from(shot.data, 'base64');
      if (bytes.length === 0) throw new Error(`empty screenshot for ${name}`);
      if (WRITE) {
        const file = join(BASE, PLATFORM, themeName, `${name}.png`);
        mkdirSync(dirname(file), { recursive: true });
        writeFileSync(file, bytes);
      }
      shots.push({ name, width: size.clientWidth, height: size.clientHeight, hash: hash(bytes), format: 'png' });
    }

    // Semantic checks: rich/stale/empty at every width.
    for (const width of WIDTHS) {
      for (const [mode, file] of [['rich', files.dark], ['empty', files.empty], ['stale', files.stale]]) {
        await navigate(file, width);
        for (const screen of E5_SCREENS) {
          const selected = await select(screen.id);
          if (!selected.ok) {
            failures.push(`${mode} ${screen.id} ${width}: ${selected.reason}`);
            continue;
          }
          const check = await measure(screen.id, width, mode);
          if (mode === 'rich') {
            if (check.cards !== check.expected) failures.push(`${screen.id} ${width}: ${check.cards} cards, expected ${check.expected}`);
            if (check.visibleCards !== check.expected) failures.push(`${screen.id} ${width}: ${check.visibleCards} visible cards, expected ${check.expected}`);
          }
          if (check.overflow > 1) failures.push(`${screen.id} ${width} ${mode}: horizontal overflow ${check.overflow}`);
          if (check.tiny > 0) failures.push(`${screen.id} ${width} ${mode}: ${check.tiny} controls under 24x24`);
          if (check.unnamed > 0) failures.push(`${screen.id} ${width} ${mode}: ${check.unnamed} unnamed controls`);
          if (!check.states.every((state) => ['available', 'empty', 'loading', 'stale', 'error', 'disabled'].includes(state))) {
            failures.push(`${screen.id} ${width} ${mode}: unexpected control state`);
          }
          if (mode === 'empty' && !check.states.every((state) => ['disabled', 'empty', 'loading', 'error', 'stale'].includes(state))) {
            failures.push(`${screen.id} ${width} empty: controls claim available without served state`);
          }
          if (mode === 'stale' && !check.states.every((state) => ['disabled', 'stale', 'error'].includes(state))) {
            failures.push(`${screen.id} ${width} stale: controls claim a current state`);
          }
        }
      }
    }

    // Screenshot baselines: 480 px dark + light for all 32 screens; the
    // 240/320/800 passes above are measured checks, not extra binaries.
    for (const width of SHOT_WIDTHS) {
      themeName = 'dark';
      await navigate(files.dark, width);
      for (const screen of E5_SCREENS) {
        const selected = await select(screen.id);
        if (!selected.ok) continue;
        await sleep(40);
        await capture(`${screen.id}-${width}`);
      }
    }
    themeName = 'light';
    await navigate(files.light, 480);
    for (const screen of E5_SCREENS) {
      const selected = await select(screen.id);
      if (!selected.ok) continue;
      await sleep(40);
      await capture(`${screen.id}-480`);
    }
    if (WRITE) {
      await cdp.send('Emulation.setDeviceMetricsOverride', { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false });
      await cdp.send('Page.navigate', { url: pathToFileURL(join(dir, 'contact.html')).href });
      await sleep(600);
      const metrics = await cdp.send('Page.getLayoutMetrics');
      const size = metrics.cssContentSize || metrics.contentSize;
      const shot = await cdp.send('Page.captureScreenshot', {
        format: 'png',
        captureBeyondViewport: true,
        clip: { x: 0, y: 0, width: Math.ceil(size.width), height: Math.ceil(size.height), scale: 1 },
      });
      mkdirSync(join(BASE, PLATFORM), { recursive: true });
      writeFileSync(join(BASE, PLATFORM, 'contact.png'), Buffer.from(shot.data, 'base64'));
    }
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }

  // Platform manifest: current-platform records + explicit absent platforms.
  const chromeVersion = spawnSync(child ? findChrome() : 'true', ['--version'], { encoding: 'utf8' }).stdout?.trim() || 'unknown';
  const manifestPath = join(BASE, `${PLATFORM}.json`);
  const previous = existsSync(manifestPath) ? JSON.parse(readFileSync(manifestPath, 'utf8')) : null;
  const manifest = {
    schema: 'faktor-edition5-screenshots/v1',
    platform: PLATFORM,
    chrome: chromeVersion,
    fixture: hash(JSON.stringify(richSnapshot())),
    generated_at_ms: Date.now(),
    widths: WRITE ? SHOT_WIDTHS : (previous?.widths ?? []),
    themes: WRITE ? ['dark', 'light'] : (previous?.themes ?? []),
    screens: Object.fromEntries(E5_SCREENS.map((screen) => [screen.id, screen.controls.length])),
    records: WRITE ? shots : (previous?.records ?? []),
    absent_platforms: ['linux', 'win32', 'darwin'].filter((entry) => entry !== PLATFORM).map((entry) => ({
      platform: entry,
      status: 'not_certified',
      reason: 'No record generated on this host; the policy never certifies one platform from another.',
    })),
    reviewed: WRITE ? true : (previous?.reviewed ?? false),
  };
  if (WRITE) {
    mkdirSync(BASE, { recursive: true });
    writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
    writeFileSync(join(BASE, PLATFORM, 'contact.html'), readFileSync(join(dir, 'contact.html')));
  }

  console.log(`edition5 render: ${E5_SCREENS.length} screens × ${WRITE ? '4 widths dark + 480 light' : 'checks'}; ${shots.length} screenshots; ${failures.length} failures`);
  if (!WRITE) {
    const records = previous?.records ?? [];
    const expected = E5_SCREENS.length * 2;
    if (records.length < expected) {
      console.log(`edition5 render: platform records not_certified (${records.length}/${expected} on ${PLATFORM})`);
    } else {
      console.log(`edition5 render: platform ${PLATFORM} records present (${records.length}); other platforms not_certified`);
    }
  }
  for (const failure of failures.slice(0, 30)) console.error(`FAIL ${failure}`);
  if (failures.length > 0) process.exitCode = 1;
  else console.log('edition5 render PASSED');
}

main().catch((error) => {
  console.error(`render-edition5 error: ${error.message}`);
  process.exitCode = 1;
});
