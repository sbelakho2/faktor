#!/usr/bin/env node
// Registry-driven "exercise every VS Code control" suite.
//
// Part A (host): each of the 200 controls is activated through the real pure
// controller with a recording Edition 5 backend. The suite asserts that every
// activation produces exactly one observable effect: a staged config value
// with its DTO, a typed command call with a native endpoint, a served re-read,
// a native host action, or an explicit refusal with no call (disabled state).
//
// Part B (webview): the real media/edition5.js runs in headless Chrome over
// the real rendered markup; every control card is clicked on every screen it
// belongs to (304 cards) and the suite asserts a postMessage carrying that
// exact control id. The script's own probe counts keyed DOM edits.
//
// Prints counts and exits nonzero on any failed control.

import { readFileSync, writeFileSync, mkdtempSync, copyFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { E5_CONTROLS, E5_SCREENS } from '../src/generated/edition5Registry.ts';
import { E5_BINDINGS, bindingOf } from '../src/edition5/bindings.ts';
import { commandCall, defaultPayload } from '../src/edition5/commands.ts';
import { activateControl, INITIAL_CONTROLLER_STATE } from '../src/edition5/host.ts';
import { renderEdition5Html } from '../src/edition5/html.ts';
import { validateSnapshot } from '../src/edition5/validate.ts';
import { richSnapshot } from './fixtures/edition5.mjs';
import { connect, findChrome, launchChrome, sleep } from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const MEDIA = join(HERE, '..', 'media');

const snapshot = validateSnapshot(richSnapshot());

class RecordingBackend {
  constructor() {
    this.calls = [];
    this.patches = [];
  }
  async read() {
    this.calls.push({ kind: 'read' });
    return snapshot;
  }
  async readConfig() {
    return snapshot.config;
  }
  async applyPatch(patch) {
    this.patches.push(patch);
    return {
      revision: 'r8',
      applied: true,
      field_states: patch.set.map((entry) => ({ key: entry.key, state: 'Effective' })),
      errors: [],
      config: snapshot.config,
    };
  }
  async execute(command, payload) {
    this.calls.push({ kind: 'execute', command, payload });
    return { ok: true, message: `ack ${command}`, call: null, body: null, errorCode: null };
  }
}

const failures = [];
function assert(condition, label) {
  if (!condition) failures.push(label);
}

const categories = new Map();
for (const control of E5_CONTROLS) {
  categories.set(control.categoryCode, (categories.get(control.categoryCode) ?? 0) + 1);
}

let hostPassed = 0;
let stagedCount = 0;
let callCount = 0;
let readCount = 0;
let hostActionCount = 0;
let refusedCount = 0;
const perCategory = new Map();

for (const control of E5_CONTROLS) {
  const binding = bindingOf(control.id);
  const backend = new RecordingBackend();
  const before = backend.calls.length;
  let outcome;
  try {
    outcome = await activateControl(backend, { ...INITIAL_CONTROLLER_STATE, snapshot }, control.id, {
      value: binding.input === 'number' || binding.input === 'money' ? '1' : undefined,
    });
  } catch (error) {
    failures.push(`${control.id}: activation threw ${error.message}`);
    continue;
  }
  const result = outcome.result;
  let ok = false;
  if (result.kind === 'staged') {
    const entry = outcome.state.draft.set.get(control.id);
    ok =
      entry !== undefined &&
      entry.key === binding.configKey &&
      binding.command === 'config.set' &&
      backend.calls.length === before;
    stagedCount += 1;
  } else if (result.kind === 'action') {
    ok = backend.calls.length === before + 2 && backend.calls[0].kind === 'execute' && backend.calls[0].command === result.command;
    // execute + re-read; the endpoint must be the typed native route.
    try {
      const planned = commandCall(result.command, {
        ...defaultPayload(result.command, snapshot),
        ...(binding.input === 'number' || binding.input === 'money' ? { value: '1' } : {}),
      });
      ok = ok && planned.path.startsWith('/native/');
      callCount += 1;
    } catch (error) {
      ok = false;
      failures.push(`${control.id}: endpoint build failed ${error.message}`);
    }
  } else if (result.kind === 'read') {
    ok = backend.calls.length === before + 1 && backend.calls[0].kind === 'read';
    readCount += 1;
  } else if (result.kind === 'host') {
    ok = binding.command === 'host.action';
    hostActionCount += 1;
  } else if (result.kind === 'refused') {
    ok = result.error !== null && backend.calls.length === before;
    refusedCount += 1;
  }
  if (!ok) {
    failures.push(`${control.id}: unexpected ${result.kind} (${result.message})`);
  } else {
    hostPassed += 1;
    perCategory.set(control.categoryCode, (perCategory.get(control.categoryCode) ?? 0) + 1);
  }
}

// The endpoint/registry must cover every binding command.
const commands = new Set([...E5_BINDINGS.values()].map((binding) => binding.command));
for (const command of commands) {
  if (command === 'config.set' || command === 'host.action') continue;
  try {
    const payload = {
      ...defaultPayload(command, snapshot),
      ...(command === 'search.transcript' ? { session: 's1' } : {}),
    };
    const call = commandCall(command, payload);
    assert(call.path.startsWith('/native/'), `${command}: route is not native`);
  } catch (error) {
    failures.push(`${command}: no valid typed call (${error.message})`);
  }
}

// ---------------------------------------------------------- browser exercise

const chrome = findChrome();
let browserActivations = 0;
let browserPassed = 0;
let uniqueBrowser = new Set();
let probe = null;
let browserSkipped = false;

if (chrome === null) {
  browserSkipped = true;
  failures.push('headless Chrome not found; the live webview exercise could not run');
} else {
  const dir = mkdtempSync(join(tmpdir(), 'faktor-e5-exercise-'));
  const scriptFile = join(dir, 'edition5.js');
  const styleFile = join(dir, 'edition5.css');
  copyFileSync(join(MEDIA, 'edition5.js'), scriptFile);
  copyFileSync(join(MEDIA, 'edition5.css'), styleFile);
  const stub = `<script nonce="N">window.__posted=[];window.acquireVsCodeApi=function(){return{postMessage:function(message){window.__posted.push(message);}};};</script>`;
  let html = renderEdition5Html({
    nonce: 'N',
    cspSource: 'file:',
    scriptUri: pathToFileURL(scriptFile).href,
    styleUri: pathToFileURL(styleFile).href,
    state: { snapshot, loading: false, error: null, focusControl: null },
    initialScreen: 'S01',
    host: 'vscode',
  });
  html = html.replace('<script nonce="N" src=', `${stub}<script nonce="N" src=`);
  const htmlFile = join(dir, 'edition5.html');
  writeFileSync(htmlFile, html);
  const child = await launchChrome(chrome);
  try {
    const cdp = await connect(child.pageWs);
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 1200,
      height: 900,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await cdp.send('Page.navigate', { url: pathToFileURL(htmlFile).href });
    await sleep(900);
    const screenMap = E5_SCREENS.map((screen) => ({ id: screen.id, controls: screen.controls }));
    const outcome = await cdp.evaluate(`(async function () {
      var screens = ${JSON.stringify(screenMap)};
      var results = [];
      function tick() { return new Promise(function (resolve) { setTimeout(resolve, 0); }); }
      for (var screen of screens) {
        var link = document.querySelector('[data-screen-link="' + screen.id + '"]');
        if (!link) { results.push({ id: screen.id, ok: false, reason: 'no screen link' }); continue; }
        link.click();
        await tick();
        for (var id of screen.controls) {
          var card = document.querySelector('[data-control="' + id + '"]');
          if (!card) { results.push({ id: id, ok: false, reason: 'no card' }); continue; }
          var before = window.__posted.length;
          var activate = card.querySelector('[data-activate]');
          var input = card.querySelector('[data-input="' + id + '"]');
          if (input && !input.disabled && input.tagName !== 'OUTPUT') {
            if (input.type === 'checkbox') {
              input.checked = !input.checked;
            } else if (input.tagName === 'SELECT') {
              var option = Array.from(input.options).find(function (entry) { return entry.value !== ''; });
              if (option) { input.value = option.value; }
            } else {
              input.value = '1';
            }
            input.dispatchEvent(new Event('change', { bubbles: true }));
            await tick();
          }
          if (activate && !activate.disabled) {
            activate.click();
          } else {
            var inspectButton = card.querySelector('[data-inspect]');
            if (inspectButton) { inspectButton.click(); }
          }
          await tick();
          var posted = window.__posted.slice(before);
          var hit = posted.find(function (message) {
            return message && (message.controlId === id || (message.set && message.set.some(function (entry) { return entry.controlId === id; })));
          });
          results.push({ id: id, screen: screen.id, ok: !!hit, state: card.getAttribute('data-state'), kind: hit ? hit.type : 'none' });
        }
      }
      // Measured incremental probes: one control and one row change, then a
      // whole-screen row change, on the real webview handler.
      var probeBefore = Object.assign({}, window.__faktorE5Probe);
      var rows = Array.from(document.querySelectorAll('#e5-screen-S01 [data-row-key]')).map(function (line) {
        return {
          key: line.getAttribute('data-row-key'),
          label: line.querySelector('.e5-row-label').textContent,
          value: line.querySelector('.e5-row-value').textContent,
          state: line.querySelector('.e5-row-value').getAttribute('data-state') || 'served',
        };
      });
      var t0 = performance.now();
      window.postMessage({
        type: 'e5Patch',
        controls: [{ id: 'M-01', state: 'available', stateText: 'served', valueText: 'ollama · user', source: 'user', disabled: false, options: ['ollama'], applyState: 'Effective' }],
        rows: {},
      }, '*');
      await tick();
      await tick();
      var t1 = performance.now();
      var oneControl = {
        controls: window.__faktorE5Probe.controls - probeBefore.controls,
        rows: window.__faktorE5Probe.rows - probeBefore.rows,
        ms: +(t1 - t0).toFixed(2),
      };
      // Establish signatures with a baseline full-row patch (not measured),
      // then measure a single-row delta against the keyed rows.
      window.postMessage({ type: 'e5Patch', controls: [], rows: { S01: rows } }, '*');
      await tick();
      await tick();
      if (rows.length > 0) {
        rows[0].value = rows[0].value + ' (delta)';
      }
      probeBefore = Object.assign({}, window.__faktorE5Probe);
      t0 = performance.now();
      window.postMessage({ type: 'e5Patch', controls: [], rows: { S01: rows } }, '*');
      await tick();
      await tick();
      t1 = performance.now();
      var oneRow = {
        controls: window.__faktorE5Probe.controls - probeBefore.controls,
        rows: window.__faktorE5Probe.rows - probeBefore.rows,
        totalRows: rows.length,
        ms: +(t1 - t0).toFixed(2),
      };
      probeBefore = Object.assign({}, window.__faktorE5Probe);
      var changedRows = rows.map(function (row) { return Object.assign({}, row, { value: row.value + ' *' }); });
      t0 = performance.now();
      window.postMessage({ type: 'e5Patch', controls: [], rows: { S01: changedRows } }, '*');
      await tick();
      await tick();
      t1 = performance.now();
      var allRows = {
        controls: window.__faktorE5Probe.controls - probeBefore.controls,
        rows: window.__faktorE5Probe.rows - probeBefore.rows,
        totalRows: rows.length,
        ms: +(t1 - t0).toFixed(2),
      };
      return {
        results: results,
        probe: window.__faktorE5Probe,
        perf: { oneControl: oneControl, oneRow: oneRow, allRows: allRows },
        controlCards: document.querySelectorAll('[data-control]').length,
        screens: document.querySelectorAll('[data-screen]').length,
      };
    })()`);
    for (const row of outcome.results) {
      browserActivations += 1;
      if (row.ok) {
        browserPassed += 1;
        uniqueBrowser.add(row.id);
      } else {
        failures.push(`${row.screen ?? '?'}/${row.id}: no observable webview effect (${row.reason ?? 'no message'})`);
      }
    }
    probe = outcome.probe;
    if (outcome.perf) console.log(`edition5 exercise: measured delta ${JSON.stringify(outcome.perf)}`);
    assert(outcome.controlCards === E5_SCREENS.reduce((sum, screen) => sum + screen.controls.length, 0), 'control card count mismatch');
    assert(outcome.screens === 32, `expected 32 screen sections, got ${outcome.screens}`);
    cdp.ws.close();
  } finally {
    child.child.kill('SIGKILL');
  }
}

// ---------------------------------------------------------------- reporting

const expectedCards = E5_SCREENS.reduce((sum, screen) => sum + screen.controls.length, 0);
console.log(`edition5 exercise: host ${hostPassed}/${E5_CONTROLS.length} controls passed (staged ${stagedCount}, calls ${callCount}, reads ${readCount}, host ${hostActionCount}, refused ${refusedCount})`);
console.log(`edition5 exercise: webview ${browserPassed}/${browserActivations} card activations passed; ${uniqueBrowser.size}/${E5_CONTROLS.length} unique controls; ${expectedCards} cards over 32 screens${browserSkipped ? ' (SKIPPED)' : ''}`);
console.log(`edition5 exercise: categories ${E5_CONTROLS.map((c) => c.categoryCode).filter((code, index, all) => all.indexOf(code) === index).map((code) => `${code}:${perCategory.get(code) ?? 0}/${categories.get(code)}`).join(' ')}`);
console.log(`edition5 exercise: webview probe ${JSON.stringify(probe)}`);
if (probe !== null) {
  assert(probe.fullRebuilds === 0, 'the webview performed a full rebuild during the exercise');
}
if (failures.length > 0) {
  for (const failure of failures.slice(0, 40)) console.error(`FAIL ${failure}`);
  console.error(`edition5 exercise FAILED: ${failures.length} issue(s)`);
  process.exitCode = 1;
} else {
  console.log('edition5 exercise PASSED: all controls have an observable effect and a typed route');
}
