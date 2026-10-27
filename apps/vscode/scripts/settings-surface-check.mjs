#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile,read-only-artifacts
// Change-scoped settings-surface verification for the Revision 8 webview.
//
// Asserts against the real DOM (headless Chrome) that the settings surface
// renders exactly one row per served descriptor, one value cell per row, six
// domains, no hidden settings screens and no visible raw descriptor key.
// It also runs a duplicate-row mutation witness and writes before/after-sized
// review captures at 240 and 480 CSS px.
//
// Usage: node scripts/settings-surface-check.mjs

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderEdition8Html } from '../src/edition8/html.ts';
import { descriptorMap } from '../src/edition8/source.ts';
import { edition8Snapshot } from './fixtures/edition8.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = join(ROOT, 'target/certification/settings');
const WIDTHS = [240, 480];

function settingsHtml(snapshot, css) {
  const rendered = renderEdition8Html({
    nonce: 'settings-check',
    cspSource: 'vscode-resource:',
    scriptUri: 'media/edition8.js',
    styleUri: 'media/edition8.css',
    state: { snapshot, loading: false, error: null, focusControl: null },
    initialScreen: 'S29',
    host: 'vscode',
  });
  return rendered.replace(
    '<link rel="stylesheet" href="media/edition8.css" />',
    `<style>${css}</style>`,
  );
}

function duplicateFirstRow(html) {
  const first = html.indexOf('data-settings-row');
  const start = html.lastIndexOf('<div class="e8-settings-row"', first);
  const next = html.indexOf('<div class="e8-settings-row"', first + 1);
  return html.slice(0, next) + html.slice(start, next) + html.slice(next);
}

function fail(message) {
  console.error(`FAIL  ${message}`);
  process.exitCode = 1;
}

async function measure(cdp, html) {
  await cdp.send('Page.navigate', { url: `data:text/html;charset=utf-8,${encodeURIComponent(html)}` });
  await sleep(120);
  return cdp.evaluate(`(function () {
    var rows = Array.from(document.querySelectorAll('[data-settings-row]'));
    return {
      rows: rows.length,
      keys: rows.map(function (row) { return row.getAttribute('data-settings-key'); }),
      values: document.querySelectorAll('.e8-settings-value').length,
      screens: document.querySelectorAll('.e8-screen').length,
      configCards: document.querySelectorAll('.e8-screen [data-config="true"]').length,
      actionRows: document.querySelectorAll('[data-settings-link-row]').length,
      domains: document.querySelectorAll('[data-settings-domain]').length,
      declared: Number(document.querySelector('[data-settings-descriptor-count]').getAttribute('data-settings-descriptor-count')),
      applies: document.querySelectorAll('#e8-apply').length,
      perScreenApply: document.querySelectorAll('[data-primary-apply]').length,
      deadControls: document.querySelectorAll('.e8-settings-row [data-activate], .e8-settings-row .e8-control-details').length,
      visibleKey: /faktor\\.[a-z0-9]+/i.test(document.body.innerText),
      overflow: document.documentElement.scrollWidth - window.innerWidth,
      height: document.documentElement.scrollHeight
    };
  })()`);
}

async function screenshot(cdp, width, path) {
  const height = await cdp.evaluate('document.documentElement.scrollHeight');
  await cdp.send('Emulation.setDeviceMetricsOverride', {
    width,
    height: Math.min(9000, Math.max(720, height)),
    deviceScaleFactor: 1,
    mobile: false,
  });
  await sleep(60);
  const shot = await cdp.send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: true });
  writeFileSync(path, Buffer.from(shot.data, 'base64'));
}

async function main() {
  const chrome = findChrome();
  if (chrome === null) {
    console.error('settings check: headless Chrome not found');
    process.exit(1);
  }
  mkdirSync(OUT, { recursive: true });
  const css = readFileSync(join(HERE, '..', 'media', 'edition8.css'), 'utf8');
  const fixture = edition8Snapshot();
  const expected = descriptorMap(fixture.descriptors).size;
  const html = settingsHtml(fixture, css);

  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  try {
    await cdp.send('Emulation.setDeviceMetricsOverride', { width: 480, height: 900, deviceScaleFactor: 1, mobile: false });
    const dom = await measure(cdp, html);
    if (dom.rows !== expected) fail(`rendered rows ${dom.rows} != served descriptors ${expected}`);
    if (new Set(dom.keys).size !== expected) fail(`duplicate descriptor rows: ${dom.rows - new Set(dom.keys).size}`);
    if (dom.values !== expected) fail(`value cells ${dom.values} != descriptors ${expected}`);
    if (dom.declared !== expected) fail(`declared count ${dom.declared} != descriptors ${expected}`);
    if (dom.configCards !== 0) fail(`config controls emitted on action pages: ${dom.configCards}`);
    if (dom.actionRows === 0) fail('no action/diagnostic pages remain reachable');
    if (dom.domains !== 6) fail(`domain groups ${dom.domains} != 6`);
    if (dom.applies !== 1) fail(`Apply footer count ${dom.applies} != 1`);
    if (dom.perScreenApply !== 0) fail(`per-screen Apply count ${dom.perScreenApply} != 0`);
    if (dom.deadControls !== 0) fail(`dead controls ${dom.deadControls} != 0`);
    if (dom.visibleKey) fail('a raw descriptor key is visible');
    if (dom.overflow > 1) fail(`horizontal overflow ${dom.overflow}px at 480`);

    // Mutation witness: the same DOM assertion must observe a duplicate row.
    const mutated = await measure(cdp, duplicateFirstRow(html));
    if (mutated.rows !== expected + 1) fail(`duplicate-row witness did not add a row (${mutated.rows})`);
    if (new Set(mutated.keys).size !== expected) fail('duplicate-row witness lost key uniqueness');
    console.log(`PASS  one row per descriptor (${dom.rows}/${expected}), 6 domains, 0 duplicate config cards, ${dom.actionRows} action pages`);
    console.log(`PASS  duplicate-row witness observed ${mutated.rows} rows for ${expected} descriptors`);

    await measure(cdp, html);
    for (const width of WIDTHS) {
      await cdp.send('Emulation.setDeviceMetricsOverride', { width, height: 900, deviceScaleFactor: 1, mobile: false });
      const shot = await measure(cdp, html);
      if (shot.overflow > 1) fail(`horizontal overflow ${shot.overflow}px at ${width}`);
      await screenshot(cdp, width, join(OUT, `after-${width}.png`));
      console.log(`PASS  ${width}px capture ${join(OUT, `after-${width}.png`)}`);
    }
  } finally {
    cdp.ws.close();
    child.child.kill('SIGKILL');
  }
  if (process.exitCode === 1) process.exit(1);
}

await main();
