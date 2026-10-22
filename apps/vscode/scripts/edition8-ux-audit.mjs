#!/usr/bin/env node
// Read-only UX measurement for the Revision 8 surface. Renders the real
// product HTML in headless Chrome, selects every screen in both destinations,
// and records the observable layout/interaction facts behind the heuristic
// scores. It does not certify anything and writes only under target/ux-audit.

import { mkdirSync, readFileSync, writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { E8_SCREENS } from '../src/generated/edition8Registry.ts';
import { destinationOf } from '../src/edition8/view.ts';
import { renderEdition8Html } from '../src/edition8/html.ts';
import { edition8Snapshot } from './fixtures/edition8.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const MEDIA = join(HERE, '..', 'media');
const OUT = resolve(ROOT, 'target/ux-audit/vscode/after');
const BEFORE = process.argv.includes('--before');

const temp = mkdtempSync(join(tmpdir(), 'faktor-e8-ux-'));
writeFileSync(join(temp, 'edition8.js'), readFileSync(join(MEDIA, 'edition8.js')));
writeFileSync(join(temp, 'edition8.css'), readFileSync(join(MEDIA, 'edition8.css')));
const theme = `<style>:root{--vscode-foreground:#d4d4d4;--vscode-editor-background:#1e1e1e;--vscode-sideBar-background:#252526;--vscode-descriptionForeground:#a7a7a7;--vscode-panel-border:#3c3c3c;--vscode-button-background:#0e639c;--vscode-button-foreground:#ffffff;--vscode-button-hoverBackground:#1177bb;--vscode-button-secondaryBackground:#3a3d41;--vscode-button-secondaryForeground:#cccccc;--vscode-input-background:#3c3c3c;--vscode-input-foreground:#cccccc;--vscode-input-border:#3c3c3c;--vscode-focusBorder:#007fd4;--vscode-editorWidget-background:#252526;--vscode-textCodeBlock-background:#2d2d2d;--vscode-errorForeground:#f14c4c;--vscode-editorWarning-foreground:#cca700;--vscode-testing-iconPassed:#73c991;--vscode-charts-blue:#3794ff;}</style>`;
const stub = `<script nonce="audit">window.__posted=[];window.acquireVsCodeApi=function(){return{postMessage:function(m){window.__posted.push(m);}};};</script>`;

function pageFor(mode, first) {
  let html = renderEdition8Html({
    nonce: 'audit',
    cspSource: '*',
    scriptUri: pathToFileURL(join(temp, 'edition8.js')).href,
    styleUri: pathToFileURL(join(temp, 'edition8.css')).href,
    state: { snapshot: edition8Snapshot(), loading: false, error: null, focusControl: null },
    initialScreen: first,
    host: 'vscode',
    mode,
  });
  html = html.replace('</head>', `${theme}</head>`);
  html = html.replace('<script nonce="audit">window.__faktorE8Initial', `${stub}<script nonce="audit">window.__faktorE8Initial`);
  const file = join(temp, `${mode}.html`);
  writeFileSync(file, html);
  return file;
}

const MEASURE = (screenId) => {
  const screen = document.querySelector(`[data-screen="${screenId}"]`);
  if (!screen || screen.hidden) return { ok: false, reason: 'screen not visible' };
  window.scrollTo(0, 0);
  const shown = (el) => {
    if (!el) return false;
    let node = el;
    while (node && node !== document.documentElement) {
      if (node.tagName === 'DETAILS' && !node.open) return false;
      node = node.parentElement;
    }
    return el.getClientRects().length > 0;
  };
  const rect = (el) => (el ? el.getBoundingClientRect() : null);
  const cards = Array.from(screen.querySelectorAll('[data-control]')).filter(shown);
  const buttons = Array.from(screen.querySelectorAll('button')).filter(shown);
  const primary = buttons.filter((b) => b.classList.contains('e8-primary'));
  const labels = buttons.map((b) => (b.textContent || '').trim()).filter(Boolean);
  const counts = new Map();
  for (const label of labels) counts.set(label, (counts.get(label) || 0) + 1);
  const duplicateActions = [...counts.values()].reduce((n, v) => n + Math.max(0, v - 1), 0);
  const host = screen.closest('.e8-destination') || screen.closest('[data-destination]');
  const browser = host?.querySelector('.e8-browser') || host?.querySelector('.e8-screens');
  const navLinks = browser ? Array.from(browser.querySelectorAll('[data-screen-link]')).filter(shown).length : 0;
  const firstControl = cards[0] || null;
  const rows = screen.querySelector('.e8-data');
  const contract = screen.querySelector('.e8-screen-contract');
  const details = screen.querySelectorAll('details').length;
  const nested = Array.from(screen.querySelectorAll('details details')).length;
  const visibleText = (screen.innerText || '').trim();
  const purpose = screen.querySelector('.e8-purpose')?.textContent?.trim() || '';
  const status = Array.from(screen.querySelectorAll('.e8-status')).filter(shown).length;
  const primaryApply = screen.querySelector('[data-primary-apply]');
  const hostAction = screen.querySelector('[data-host-command]');
  const deadEnds = (screen.querySelector('.e8-primary-row')?.textContent || '').includes('Primary action:') && !primaryApply && !hostAction ? 1 : 0;
  return {
    ok: true,
    navOpen: browser ? browser.tagName === 'DETAILS' ? browser.open : true : false,
    navHeight: browser ? Math.round(rect(browser).height) : 0,
    navLinks,
    screenTop: Math.round(rect(screen).top),
    firstControlTop: firstControl ? Math.round(rect(firstControl).top) : null,
    controls: cards.length,
    buttons: buttons.length,
    primaryButtons: primary.length,
    duplicateActions,
    genericActions: labels.filter((label) => label === 'Inspect served state').length,
    contractChars: contract && shown(contract) ? (contract.textContent || '').trim().length : 0,
    purposeChars: purpose.length,
    rows: rows ? rows.querySelectorAll('[data-row-key]').length : 0,
    rowsBeforeControls: Boolean(rows && firstControl && rect(rows).top < rect(firstControl).top),
    details,
    nestedDetails: nested,
    statusLines: status,
    deadEnds,
    visibleTextChars: visibleText.length,
  };
};

const chrome = findChrome();
if (!chrome) {
  console.error('ux audit: headless Chrome not found');
  process.exit(1);
}
const child = await launchChrome(chrome);
const cdp = await connect(child.pageWs);
await cdp.send('Page.enable');
await cdp.send('Emulation.setDeviceMetricsOverride', { width: 480, height: 900, deviceScaleFactor: 1, mobile: false });
await cdp.send('Runtime.enable');

const modes = [
  ['shell', E8_SCREENS.filter((s) => destinationOf(s) !== 'settings')],
  ['settings', E8_SCREENS.filter((s) => destinationOf(s) === 'settings')],
];
const records = [];
for (const [mode, screensForMode] of modes) {
  const file = pageFor(mode, screensForMode[0].id);
  const loaded = cdp.waitFor('Page.loadEventFired');
  await cdp.send('Page.navigate', { url: pathToFileURL(file).href });
  await loaded;
  await cdp.evaluate('new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))');
  for (const screen of screensForMode) {
    const result = await cdp.evaluate(`(() => {
      const click = document.querySelector('[data-screen-link="${screen.id}"]');
      if (click) click.click();
      return (${MEASURE.toString()})(${JSON.stringify(screen.id)});
    })()`);
    if (!result.ok) {
      records.push({ id: screen.id, title: screen.title, group: screen.group, error: result.reason });
      continue;
    }
    const m = result;
    const clarity = Math.max(1, Math.min(5, 5
      - (m.navLinks > 8 ? 1 : 0)
      - (m.primaryButtons > 1 ? 1 : 0)
      - (m.contractChars > 240 ? 1 : 0)
      - (m.firstControlTop !== null && m.firstControlTop > 640 ? 1 : 0)
      - (m.deadEnds > 0 ? 1 : 0)));
    const density = Math.max(1, Math.min(5, 5
      - Math.floor(m.controls / 9)
      - (m.rowsBeforeControls ? 1 : 0)
      - (m.navLinks > 8 ? 1 : 0)));
    const consistency = Math.max(1, Math.min(5, 5
      - (m.duplicateActions > 0 ? 1 : 0)
      - (m.genericActions > 0 ? 1 : 0)
      - (m.statusLines !== 1 ? 1 : 0)));
    const efficiency = Math.max(1, Math.min(5, 5
      - (m.firstControlTop !== null && m.firstControlTop > 480 ? 1 : 0)
      - (m.navLinks > 8 ? 1 : 0)
      - (m.primaryButtons > 1 ? 1 : 0)
      - (m.primaryButtons === 0 && m.deadEnds > 0 ? 1 : 0)));
    const recovery = Math.max(1, Math.min(5, 5
      - (m.statusLines === 0 ? 1 : 0)
      - (m.details === 0 ? 2 : 0)
      - (m.controls > 0 && m.details === 0 ? 1 : 0)));
    const problems = [];
    if (m.navLinks > 8) problems.push(`${m.navLinks} screen links precede the screen`);
    if (m.primaryButtons > 1) problems.push(`${m.primaryButtons} competing primary buttons`);
    if (m.contractChars > 240) problems.push(`${m.contractChars} chars of contract text above controls`);
    if (m.rowsBeforeControls) problems.push(`${m.rows} served rows precede the controls`);
    if (m.firstControlTop > 640) problems.push(`first control at ${m.firstControlTop}px (below the fold)`);
    if (m.deadEnds > 0) problems.push('named primary action has no control on this surface');
    if (m.duplicateActions > 0) problems.push(`${m.duplicateActions} duplicate visible action labels`);
    if (m.genericActions > 0) problems.push(`${m.genericActions} generic "Inspect served state" action labels`);
    records.push({
      id: screen.id,
      title: screen.title,
      group: screen.group,
      metrics: m,
      score: { clarity, density, consistency, efficiency, recovery },
      problems,
    });
  }
}

const scores = records.filter((r) => r.score);
const mean = (key) => Math.round((scores.reduce((sum, r) => sum + r.score[key], 0) / Math.max(1, scores.length)) * 100) / 100;
const outDir = BEFORE ? resolve(ROOT, 'target/ux-audit/vscode/before') : OUT;
mkdirSync(outDir, { recursive: true });
const document = {
  schema: 'faktor-vscode-edition8-ux-audit/v1',
  generatedAt: new Date().toISOString(),
  screens: records.length,
  means: { clarity: mean('clarity'), density: mean('density'), consistency: mean('consistency'), efficiency: mean('efficiency'), recovery: mean('recovery') },
  records,
};
writeFileSync(join(outDir, 'ux-metrics.json'), `${JSON.stringify(document, null, 2)}\n`);
const csv = ['id,title,group,clarity,density,consistency,efficiency,recovery,firstControlTop,controls,primaryButtons,duplicateActions,contractChars,rowsBeforeControls'];
for (const r of records) {
  if (!r.score) continue;
  csv.push([r.id, JSON.stringify(r.title), JSON.stringify(r.group), r.score.clarity, r.score.density, r.score.consistency, r.score.efficiency, r.score.recovery, r.metrics.firstControlTop ?? '', r.metrics.controls, r.metrics.primaryButtons, r.metrics.duplicateActions, r.metrics.contractChars, r.metrics.rowsBeforeControls ? 1 : 0].join(','));
}
writeFileSync(join(outDir, 'ux-scores.csv'), `${csv.join('\n')}\n`);
console.log(`${BEFORE ? 'before' : 'after'}: ${records.length} screens · clarity ${document.means.clarity} density ${document.means.density} consistency ${document.means.consistency} efficiency ${document.means.efficiency} recovery ${document.means.recovery}`);
child.child.kill('SIGKILL');
process.exit(0);
