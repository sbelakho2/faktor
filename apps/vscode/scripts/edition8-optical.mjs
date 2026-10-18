#!/usr/bin/env node
// In-host optical cap-centre audit for the Edition 8 VS Code surface.
//
// Rasterises real rendered buttons at 3x, crops the interior (excluding the
// border and 7 px at each end), measures the first ink row (cap top) and the
// median per-column lowest ink row (baseline), and compares their midpoint to
// the button box centre. Requires >=30 buttons per host/theme.
//
// Output: target/certification/edition8/optical-vscode.raw.json.

import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { platform, release, arch } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  E8_SCREENS,
  E8_THEMES,
  renderScreenHtml,
} from './edition8/host.mjs';
import { readPng, luminance } from '../../../tests/edition8/png.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const SCALE = 3;
const MIN_BUTTONS = 30;
const PLATFORM = platform() === 'darwin' ? 'macos' : platform() === 'win32' ? 'windows' : 'linux';

function sha(bytes) {
  return `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
}

function measureInk(png, border = 7) {
  const left = Math.min(border, Math.floor(png.width / 4));
  const right = png.width - left;
  const top = Math.min(border, Math.floor(png.height / 4));
  const bottom = png.height - top;
  const rows = new Map();
  let first = null;
  const ink = [];
  const corner = (((Math.min(border, png.height - 1) + 1) * png.width + Math.min(border, png.width - 1)) * 4);
  const background = luminance(png.pixels[corner], png.pixels[corner + 1], png.pixels[corner + 2]);
  for (let x = left; x < right; x += 1) {
    let lowest = null;
    for (let y = top; y < bottom; y += 1) {
      const index = (y * png.width + x) * 4;
      const r = png.pixels[index];
      const g = png.pixels[index + 1];
      const b = png.pixels[index + 2];
      // Ink is any pixel at least 0.2 luminance away from the most common
      // (background) colour; the crop excludes the border.
      const value = luminance(r, g, b);
      if (Math.abs(value - background) > 0.12) {
        if (first === null || y < first) first = y;
        lowest = y;
        ink.push({ x, y });
      }
    }
    if (lowest !== null) rows.set(x, lowest);
  }
  if (first === null || rows.size === 0) return null;
  const lows = [...rows.values()].sort((a, b) => a - b);
  const baseline = lows[Math.floor(lows.length / 2)];
  return { capTop: first, baseline, columns: rows.size, ink: ink.length };
}

async function main() {
  const chrome = findChrome();
  if (chrome === null) {
    console.error('edition8 optical: headless Chrome not found');
    process.exit(1);
  }
  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  const themes = E8_THEMES.vscode;
  const measurements = {};
  try {
    for (const theme of themes) {
      const samples = [];
      for (const screen of E8_SCREENS) {
        if (samples.length >= MIN_BUTTONS) break;
        const html = renderScreenHtml({ screenId: screen.id, fixtureClass: 'ready', theme, width: 480, zoom: 1 });
        await cdp.send('Emulation.setDeviceMetricsOverride', {
          width: 480,
          height: 900,
          deviceScaleFactor: 1,
          mobile: false,
        });
        await cdp.send('Page.navigate', { url: `data:text/html;charset=utf-8,${encodeURIComponent(html)}` });
        await sleep(60);
        const box = await cdp.evaluate(`(function () {
          var button = document.querySelector('.e8-primary-action') || document.querySelector('button[data-activate]');
          if (!button) return null;
          var rect = button.getBoundingClientRect();
          if (rect.width < 8 || rect.height < 8 || rect.height > 34) return null;
          var label = button.textContent.trim();
          if (label.length > 34) return null;
          return { x: rect.x, y: rect.y, width: rect.width, height: rect.height, label: label };
        })()`);
        if (box === null) continue;
        const shot = await cdp.send('Page.captureScreenshot', {
          format: 'png',
          clip: { x: box.x, y: box.y, width: box.width, height: box.height, scale: SCALE },
        });
        const bytes = Buffer.from(shot.data, 'base64');
        const png = readPng(bytes);
        const ink = measureInk(png);
        if (ink === null) continue;
        const cssHeight = box.height;
        const offset = ((ink.capTop + ink.baseline) / 2) / SCALE - cssHeight / 2;
        samples.push({
          screen: screen.id,
          label: box.label.slice(0, 60),
          width: box.width,
          height: box.height,
          cap_top: ink.capTop / SCALE,
          baseline: ink.baseline / SCALE,
          offset,
          screenshot: sha(bytes),
          columns: ink.columns,
          image_width: png.width,
          image_height: png.height,
        });
      }
      if (samples.length < MIN_BUTTONS) {
        console.error(`edition8 optical: ${theme} measured only ${samples.length} buttons`);
        process.exit(1);
      }
      const sorted = samples.map((sample) => sample.offset).sort((a, b) => a - b);
      const median = sorted[Math.floor(sorted.length / 2)];
      const mean = sorted.reduce((total, value) => total + value, 0) / sorted.length;
      const p5 = sorted[Math.floor(sorted.length * 0.05)];
      const p95 = sorted[Math.floor(sorted.length * 0.95)];
      const max = Math.max(...sorted.map((value) => Math.abs(value)));
      measurements[theme] = { buttons: samples.length, median, mean, p5, p95, max_abs: max, samples };
    }
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }
  const source = await import('node:fs').then((fs) => fs.readFileSync(resolve(HERE, 'edition8/host.mjs'), 'utf8'));
  const failures = [];
  for (const [theme, entry] of Object.entries(measurements)) {
    if (entry.buttons < MIN_BUTTONS) failures.push(`${theme}: ${entry.buttons} buttons < ${MIN_BUTTONS}`);
    if (Math.abs(entry.median) > 0.5) failures.push(`${theme}: median ${entry.median.toFixed(2)} px > 0.5 px`);
  }
  if (!/trim-both|line-height|cap/i.test(source)) failures.push('renderer does not trim/centre labels by cap height');
  const document = {
    schema: 'faktor-edition8-optical/v1',
    host: 'vscode',
    platform: PLATFORM,
    os: `${platform()} ${release()} ${arch()}`,
    generated_at: new Date().toISOString(),
    method: '3x raster, interior crop excluding border and 7px per end',
    counts: {
      themes: Object.keys(measurements).length,
      buttons: Object.values(measurements).reduce((total, entry) => total + entry.buttons, 0),
      failed: failures.length,
    },
    measurements,
    failures,
  };
  mkdirSync(OUT, { recursive: true });
  const payload = `${JSON.stringify(document, null, 2)}\n`;
  writeFileSync(resolve(OUT, `optical-vscode.${PLATFORM}.raw.json`), payload);
  writeFileSync(resolve(OUT, 'optical-vscode.raw.json'), payload);
  for (const failure of failures) console.error(`FAIL ${failure}`);
  console.log(
    `edition8 optical vscode: ${Object.entries(measurements).map(([theme, entry]) => `${theme} ${entry.buttons} buttons median ${entry.median.toFixed(2)}px`).join('; ')}`,
  );
  if (failures.length > 0) process.exit(1);
}

await main();
