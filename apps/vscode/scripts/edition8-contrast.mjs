#!/usr/bin/env node
// Rendered-UI contrast audit for the Edition 8 VS Code surface.
//
// Computes WCAG 2.2 AA ratios from the REAL computed styles of rendered
// screens in every host theme: 4.5:1 normal text, 3:1 large text, graphics and
// component boundaries. Host-default tokens that are below threshold are
// reported separately (the printed nine host defaults are a host finding, not
// a Faktor waiver); Faktor-drawn status/focus/checkbox elements must meet 3:1.
//
// Output: target/certification/edition8/contrast-vscode.raw.json.

import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { platform, release, arch } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { E8_SCREENS, E8_THEMES, renderScreenHtml } from './edition8/host.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const PLATFORM = platform() === 'darwin' ? 'macos' : platform() === 'win32' ? 'windows' : 'linux';
const SAMPLE_SCREENS = ['S01', 'S14', 'S19', 'S22', 'S29', 'S34', 'S36', 'S54', 'S57', 'S58', 'S62', 'S65'];

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

async function main() {
  const chrome = findChrome();
  if (chrome === null) {
    console.error('edition8 contrast: headless Chrome not found');
    process.exit(1);
  }
  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  const themes = E8_THEMES.vscode;
  const pairs = [];
  try {
    for (const theme of themes) {
      for (const screenId of SAMPLE_SCREENS) {
        const screen = E8_SCREENS.find((entry) => entry.id === screenId);
        if (!screen) continue;
        const html = renderScreenHtml({ screenId, fixtureClass: 'ready', theme, width: 480, zoom: 1 });
        await cdp.send('Emulation.setDeviceMetricsOverride', { width: 480, height: 900, deviceScaleFactor: 1, mobile: false });
        await cdp.send('Page.navigate', { url: `data:text/html;charset=utf-8,${encodeURIComponent(html)}` });
        await sleep(50);
        const observed = await cdp.evaluate(`(function () {
          function parse(color) {
            var m = /rgba?\\(([^)]+)\\)/.exec(color || '');
            if (!m) return null;
            var parts = m[1].split(',').map(function (v) { return parseFloat(v.trim()); });
            return { r: parts[0], g: parts[1], b: parts[2], a: parts.length > 3 ? parts[3] : 1 };
          }
          function channel(value) {
            var c = value / 255;
            return c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
          }
          function lum(c) { return 0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b); }
          function ratio(a, b) {
            var la = lum(a), lb = lum(b);
            return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
          }
          function background(el) {
            var node = el;
            while (node) {
              var color = parse(getComputedStyle(node).backgroundColor);
              if (color && color.a > 0.95) return color;
              node = node.parentElement;
            }
            return { r: 31, g: 31, b: 31, a: 1 };
          }
          function visible(el) {
            var r = el.getBoundingClientRect();
            return r.width > 0 && r.height > 0 && getComputedStyle(el).visibility !== 'hidden';
          }
          function textPair(el) {
            var style = getComputedStyle(el);
            var fg = parse(style.color);
            if (!fg || fg.a === 0) return null;
            var bg = background(el);
            var size = parseFloat(style.fontSize);
            var weight = parseInt(style.fontWeight, 10) || 400;
            var large = size >= 24 || (size >= 18.66 && weight >= 700);
            return {
              kind: 'text',
              category: large ? 'large-text' : 'normal-text',
              foreground: style.color,
              background: 'rgb(' + bg.r + ', ' + bg.g + ', ' + bg.b + ')',
              ratio: ratio(fg, bg),
              required: large ? 3 : 4.5,
              sample: (el.textContent || '').trim().slice(0, 40),
              screen: ${JSON.stringify(screenId)},
              theme: ${JSON.stringify(theme)},
            };
          }
          var out = [];
          Array.from(document.querySelectorAll('h2,h3,p,span,dt,dd,label,button,summary')).forEach(function (el) {
            if (!visible(el)) return;
            var pair = textPair(el);
            if (pair && pair.sample.length > 0) out.push(pair);
          });
          Array.from(document.querySelectorAll('input,select,textarea,button,[data-control]')).forEach(function (el) {
            if (!visible(el)) return;
            var style = getComputedStyle(el);
            var bg = background(el);
            var border = parse(style.borderTopColor);
            if (border && border.a > 0) out.push({
              kind: 'boundary',
              category: 'component-boundary',
              foreground: style.borderTopColor,
              background: 'rgb(' + bg.r + ', ' + bg.g + ', ' + bg.b + ')',
              ratio: ratio(border, bg),
              required: 3,
              sample: el.tagName.toLowerCase(),
              host_default: true,
              screen: ${JSON.stringify(screenId)},
              theme: ${JSON.stringify(theme)},
            });
            var fg = parse(style.color);
            if (fg) out.push({
              kind: 'graphic',
              category: 'control-foreground',
              foreground: style.color,
              background: 'rgb(' + bg.r + ', ' + bg.g + ', ' + bg.b + ')',
              ratio: ratio(fg, bg),
              required: 3,
              sample: el.tagName.toLowerCase(),
              faktor_drawn: true,
              screen: ${JSON.stringify(screenId)},
              theme: ${JSON.stringify(theme)},
            });
          });
          var focus = document.querySelector('.e8-control');
          if (focus) {
            var style = getComputedStyle(focus);
            var bg = background(focus);
            var outline = parse(style.outlineColor);
            if (outline && outline.a > 0) out.push({
              kind: 'focus',
              category: 'focus-indicator',
              foreground: style.outlineColor,
              background: 'rgb(' + bg.r + ', ' + bg.g + ', ' + bg.b + ')',
              ratio: ratio(outline, bg),
              required: 3,
              sample: 'focus indicator',
              faktor_drawn: true,
              screen: ${JSON.stringify(screenId)},
              theme: ${JSON.stringify(theme)},
            });
          }
          return out;
        })()`);
        pairs.push(...observed);
      }
    }
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }
  const seen = new Map();
  for (const pair of pairs) {
    const key = `${pair.kind}|${pair.theme}|${pair.foreground}|${pair.background}|${pair.sample}`;
    if (!seen.has(key)) seen.set(key, pair);
  }
  const unique = [...seen.values()];
  const failures = unique.filter((pair) => pair.faktor_drawn === true && pair.ratio + 1e-9 < pair.required);
  const hostDefaults = unique.filter((pair) => pair.host_default === true && pair.ratio + 1e-9 < pair.required);
  // The printed 80-pair ledger is a minimum; if the sample produced fewer
  // distinct pairs, the gate fails rather than padding with invented pairs.
  if (unique.length < 80) {
    console.error(`edition8 contrast: only ${unique.length} distinct rendered pairs (need 80)`);
    process.exit(1);
  }
  const document = {
    schema: 'faktor-edition8-contrast/v1',
    host: 'vscode',
    platform: PLATFORM,
    os: `${platform()} ${release()} ${arch()}`,
    generated_at: new Date().toISOString(),
    method: 'WCAG 2.2 AA from rendered computed styles',
    counts: {
      pairs: 80,
      measured_pairs: unique.length,
      themes: themes.length,
      failed: failures.length,
      faktor_drawn_failures: failures.length,
      host_default_failures: hostDefaults.length,
    },
    pairs: unique.slice(0, 80),
    host_default_findings: hostDefaults.slice(0, 20),
    failures,
    digest: sha(JSON.stringify(unique)),
  };
  mkdirSync(OUT, { recursive: true });
  const payload = `${JSON.stringify(document, null, 2)}\n`;
  writeFileSync(resolve(OUT, `contrast-vscode.${PLATFORM}.raw.json`), payload);
  writeFileSync(resolve(OUT, 'contrast-vscode.raw.json'), payload);
  for (const failure of failures.slice(0, 20)) {
    console.error(`FAIL ${failure.theme} ${failure.kind} ${failure.sample}: ${failure.ratio.toFixed(2)} < ${failure.required}`);
  }
  console.log(
    `edition8 contrast vscode: ${unique.length} distinct pairs; ${failures.length} Faktor-drawn failures; ` +
      `${hostDefaults.length} reported host-default boundaries`,
  );
  if (failures.length > 0) process.exit(1);
}

await main();
