#!/usr/bin/env node
// Live-streaming performance probe for the Faktor VS Code webview.
//
// Unlike scripts/selftest.mjs (a correctness harness with a fake DOM), this
// drives the REAL media/chat.css + media/chat.js in headless Chrome and
// counts, per stream delta:
//   - messages delivered to the webview (window message events)
//   - render passes (the chat.js render seam) and coalesced messages
//   - transcript rebuild/append/last-node passes
//   - DOM nodes under #entries
//   - layout reads/writes (scrollTop/scrollHeight/offsetTop/... accessors)
//   - querySelectorAll scans and DOM mutations
//   - Chrome layout/recalc/script counters
//
// Scenario: a 500-message transcript with one growing assistant entry,
// 10,000 streamed tokens delivered as 500 deltas (20 tokens each) at
// 200 deltas/s, first pinned at the bottom and then with the reader scrolled
// away from the bottom. Usage:
//
//   node scripts/stream-probe.mjs --label before
//   node scripts/stream-probe.mjs --label after --compare before
//
// Results are written to scripts/baselines/stream-probe-<label>.json.

import { mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  buildHarnessHtml,
  connect,
  findChrome,
  launchChrome,
  sleep,
} from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT_DIR = join(HERE, 'baselines');

const PROBE_SCRIPT = `(function () {
  var probe = (window.__faktorPerf = {
    messages: 0,
    snapshotMessages: 0,
    layoutReads: 0,
    layoutWrites: 0,
    queries: 0,
    mutations: 0,
    frames: 0,
    renderCounts: {},
  });
  function patchGetter(proto, name) {
    try {
      var descriptor = Object.getOwnPropertyDescriptor(proto, name);
      if (!descriptor) {
        return;
      }
      var next = { configurable: true, enumerable: descriptor.enumerable };
      if (descriptor.get) {
        next.get = function () {
          probe.layoutReads += 1;
          return descriptor.get.call(this);
        };
      }
      if (descriptor.set) {
        next.set = function (value) {
          probe.layoutWrites += 1;
          return descriptor.set.call(this, value);
        };
      }
      Object.defineProperty(proto, name, next);
    } catch (error) {
      // A non-configurable host accessor is simply not instrumented.
    }
  }
  ['scrollTop', 'scrollHeight', 'clientHeight', 'clientWidth', 'scrollLeft'].forEach(function (name) {
    patchGetter(Element.prototype, name);
  });
  ['offsetTop', 'offsetHeight', 'offsetWidth'].forEach(function (name) {
    patchGetter(HTMLElement.prototype, name);
  });
  try {
    var rect = Element.prototype.getBoundingClientRect;
    Element.prototype.getBoundingClientRect = function () {
      probe.layoutReads += 1;
      return rect.apply(this, arguments);
    };
  } catch (error) {}
  try {
    var query = Element.prototype.querySelectorAll;
    Element.prototype.querySelectorAll = function () {
      probe.queries += 1;
      return query.apply(this, arguments);
    };
    var queryOne = Element.prototype.querySelector;
    Element.prototype.querySelector = function () {
      probe.queries += 1;
      return queryOne.apply(this, arguments);
    };
  } catch (error) {}
  try {
    var raf = window.requestAnimationFrame.bind(window);
    window.requestAnimationFrame = function (callback) {
      probe.frames += 1;
      return raf(callback);
    };
  } catch (error) {}
  try {
    new MutationObserver(function (records) {
      probe.mutations += records.length;
    }).observe(document.documentElement, {
      subtree: true,
      childList: true,
      characterData: true,
      attributes: true,
    });
  } catch (error) {}
  window.__faktorRenderProbe = {
    count: function (kind) {
      probe.renderCounts[kind] = (probe.renderCounts[kind] || 0) + 1;
    },
    value: function (kind, value) {
      probe.values = probe.values || {};
      probe.values[kind] = value;
      probe.series = probe.series || {};
      probe.series[kind] = probe.series[kind] || [];
      probe.series[kind].push(value);
      if (probe.series[kind].length > 500) {
        probe.series[kind].shift();
      }
    },
  };
  window.addEventListener(
    'message',
    function (event) {
      probe.messages += 1;
      if (event.data && event.data.type === 'snapshot') {
        probe.snapshotMessages += 1;
      }
    },
    true,
  );
})();`;

function parseArgs(argv) {
  const args = { label: 'run', compare: null };
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--label') {
      args.label = argv[++i];
    } else if (argv[i] === '--compare') {
      args.compare = argv[++i];
    }
  }
  return args;
}

const DRIVER = `(function () {
  var TOKENS_PER_DELTA = 20;
  var DELTAS = 500;
  var BASE_MESSAGES = 499;
  var transcript = [];
  function filler(index) {
    return 'Message ' + index + ': ' + 'durable transcript line for the probe. '.repeat(4);
  }
  for (var index = 1; index <= BASE_MESSAGES; index += 1) {
    transcript.push({
      id: 'm-' + index,
      role: index % 2 === 0 ? 'assistant' : 'user',
      seq: index,
      createdMs: index,
      text: filler(index),
      reasoning: '',
      summary: '',
      tools: [],
    });
  }
  transcript.push({
    id: 'm-stream',
    role: 'assistant',
    seq: BASE_MESSAGES + 1,
    createdMs: BASE_MESSAGES + 1,
    text: '',
    reasoning: '',
    summary: '',
    tools: [
      {
        toolCallId: 'tool-1',
        name: 'bash',
        state: 'running',
        input: null,
        excerpt: '',
        exitCode: null,
        artifact: null,
      },
    ],
  });
  var tokens = 0;
  var deltas = 0;
  var revision = 0;
  var scrollSamples = [];
  function snapshot() {
    return {
      daemon: 'running',
      daemonDetail: 'http://127.0.0.1:7337',
      baseUrl: 'http://127.0.0.1:7337',
      session: { id: 'p1', title: 'Probe session', provider: 'openai', model: 'gpt-5', state: 'running' },
      machineState: 'running',
      machineLabel: 'Running',
      sessions: [],
      sessionsUnavailable: null,
      workspaceName: 'probe',
      runs: [{ taskId: 't1', runId: 'r1', mode: 'work', state: 'running', goal: 'stream probe', model: null }],
      activeRunId: 'r1',
      proofTaskState: null,
      agents: [],
      permissions: [],
      task: {
        goal: 'stream probe',
        state: 'running',
        completed: ['one'],
        open: ['two'],
        testsRun: [],
        testsFailed: [],
        changedFiles: [],
        budget: null,
        acceptanceCriteria: [],
        plan: [],
        blockers: [],
        evidenceRefs: [],
        phase: 'work',
        progress: null,
        completion: null,
        contract: null,
        contractMalformed: null,
        contractDroppedRows: 0,
      },
      verification: null,
      usage: null,
      usagePanel: null,
      cockpit: { state: 'ok' },
      cockpitSections: [{ key: 'task', title: 'Task', present: true, lines: ['stream probe'], evidence: [], actions: [] }],
      tournament: null,
      board: null,
      changes: null,
      outcomes: null,
      indexCoverage: null,
      transcript: transcript,
      transcriptRevision: 'rev-' + revision,
      streamStatus: 'open',
      preflight: null,
      lastError: null,
      busy: true,
    };
  }
  function post() {
    window.postMessage({ type: 'snapshot', snapshot: snapshot() }, '*');
  }
  function seed() {
    post();
  }
  function step() {
    var streamed = transcript[transcript.length - 1];
    var words = [];
    for (var index = 0; index < TOKENS_PER_DELTA; index += 1) {
      words.push('tok' + (tokens + index));
    }
    tokens += TOKENS_PER_DELTA;
    deltas += 1;
    revision += 1;
    streamed.text += (streamed.text.length === 0 ? '' : ' ') + words.join(' ');
    streamed.tools[0].excerpt = streamed.text.slice(0, 240);
    post();
    if (deltas % 10 === 0) {
      var scroller = document.getElementById('entries');
      scrollSamples.push({
        d: deltas,
        top: Math.round(scroller.scrollTop),
        height: Math.round(scroller.scrollHeight),
        children: scroller.children.length,
        selection: String(window.getSelection ? window.getSelection() : ''),
      });
    }
    if (deltas >= DELTAS) {
      clearInterval(timer);
      window.__probeDone = true;
    }
  }
  var timer = null;
  window.__probe = {
    seed: seed,
    begin: function (pinned, select) {
      var entries = document.getElementById('entries');
      if (pinned) {
        entries.scrollTop = entries.scrollHeight - entries.clientHeight;
      } else {
        // A real reader scrolls with an input gesture; the wheel marks the
        // following scroll event as user intent (the explicit pin state
        // machine ignores programmatic/browser-internal scrolls).
        entries.dispatchEvent(new WheelEvent('wheel', { deltaY: -320, bubbles: true }));
        entries.scrollTop = 0;
      }
      if (select) {
        // Put real text in the streamed entry, let the coalesced snapshot
        // render it, then select part of it. Every subsequent delta patches
        // THIS node in place; the selection must survive the patch.
        window.__probeStart = Date.now();
        var streamed = transcript[transcript.length - 1];
        streamed.text = 'stream start';
        post();
        var attempts = 0;
        var selectWhenRendered = function () {
          attempts += 1;
          var last = entries.lastElementChild;
          var part = last && last.querySelector ? last.querySelector('.entry-text') : null;
          if (part && part.firstChild && part.firstChild.nodeType === 3) {
            var range = document.createRange();
            range.setStart(part.firstChild, 0);
            range.setEnd(part.firstChild, Math.min(6, part.firstChild.nodeValue.length));
            var selection = window.getSelection();
            selection.removeAllRanges();
            selection.addRange(range);
            window.__probeSelectionInitial = String(selection);
            window.__probeSelectionAt = Date.now() - window.__probeStart;
            return;
          }
          if (attempts < 120) {
            setTimeout(selectWhenRendered, 16);
          }
        };
        setTimeout(selectWhenRendered, 0);
      }
      tokens = 0;
      deltas = 0;
      scrollSamples = [
        {
          d: 'begin',
          top: Math.round(entries.scrollTop),
          height: Math.round(entries.scrollHeight),
          overflowAnchor: getComputedStyle(entries).overflowAnchor,
          selection: String(window.getSelection ? window.getSelection() : ''),
          initial: window.__probeSelectionInitial || null,
        },
      ];
      requestAnimationFrame(function () {
        scrollSamples.push({
          d: 'raf0',
          top: Math.round(entries.scrollTop),
          height: Math.round(entries.scrollHeight),
        });
      });
      window.__probeDone = false;
      setTimeout(function () {
        scrollSamples.push({
          d: 'idle50',
          top: Math.round(entries.scrollTop),
          height: Math.round(entries.scrollHeight),
        });
      }, 50);
      setTimeout(function () {
        scrollSamples.push({
          d: 'idle100',
          top: Math.round(entries.scrollTop),
          height: Math.round(entries.scrollHeight),
          active: document.activeElement ? document.activeElement.tagName + '#' + document.activeElement.id : null,
          selection: String(window.getSelection ? window.getSelection() : ''),
          initial: window.__probeSelectionInitial || null,
        });
        timer = setInterval(step, 5);
      }, 100);
      return { tokens: TOKENS_PER_DELTA * DELTAS, deltas: DELTAS };
    },
    state: function () {
      var entries = document.getElementById('entries');
      var nodes = 0;
      var walker = document.createTreeWalker(entries, NodeFilter.SHOW_ELEMENT);
      while (walker.nextNode()) {
        nodes += 1;
      }
      return {
        done: window.__probeDone === true,
        nodes: nodes,
        children: entries.children.length,
        scrollTop: entries.scrollTop,
        scrollHeight: entries.scrollHeight,
        clientHeight: entries.clientHeight,
        scrollSamples: scrollSamples,
        active: (function () {
          var active = document.activeElement;
          return active
            ? {
                tag: active.tagName,
                id: active.id,
                inEntries: document.getElementById('entries').contains(active),
              }
            : null;
        })(),
        selectionInitial: window.__probeSelectionInitial || null,
        selectionAt: window.__probeSelectionAt || null,
        selection: (function () {
          var selection = window.getSelection();
          if (!selection || selection.rangeCount === 0) {
            return { present: false };
          }
          var range = selection.getRangeAt(0);
          return {
            present: true,
            collapsed: range.collapsed,
            text: String(selection),
            start: range.startOffset,
            end: range.endOffset,
            inEntries:
              entries.contains(range.startContainer) && entries.contains(range.endContainer),
          };
        })(),
      };
    },
  };
})();`;

function metricMap(metrics) {
  const out = {};
  for (const metric of metrics) {
    out[metric.name] = metric.value;
  }
  return out;
}

async function readProbe(cdp) {
  return cdp.evaluate('window.__faktorPerf');
}

async function chromeMetrics(cdp) {
  const result = await cdp.send('Performance.getMetrics');
  return metricMap(result.metrics);
}

function delta(after, before) {
  const out = {};
  for (const key of Object.keys(after)) {
    if (typeof after[key] === 'number' && typeof before[key] === 'number') {
      out[key] = Math.round((after[key] - before[key]) * 1000) / 1000;
    } else {
      out[key] = after[key];
    }
  }
  return out;
}

async function runScenario(cdp, url, pinned, select = false) {
  await cdp.send('Page.navigate', { url });
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (await cdp.evaluate('window.__ready === true')) {
      break;
    }
    await sleep(25);
  }
  await cdp.evaluate(DRIVER);
  const empty = await readProbe(cdp);
  await cdp.evaluate('window.__faktorPerf.messages = 0; window.__faktorPerf.snapshotMessages = 0; window.__faktorPerf.layoutReads = 0; window.__faktorPerf.layoutWrites = 0; window.__faktorPerf.queries = 0; window.__faktorPerf.mutations = 0; window.__faktorPerf.frames = 0; window.__faktorPerf.renderCounts = {};');
  await cdp.evaluate('window.__probe.seed()');
  // Wait for the coalesced snapshot flush (message task -> frame), not just
  // an arbitrary frame: a fixed frame count can resolve before the snapshot
  // task, leaving the "seed" render to race the scenario.
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const seeded = await cdp.evaluate(
      '(window.__faktorPerf.renderCounts.snapshot || 0) > 0 && document.getElementById("entries").children.length > 0',
    );
    if (seeded) {
      break;
    }
    await sleep(25);
  }
  await cdp.evaluate('new Promise(function (resolve) { requestAnimationFrame(function () { requestAnimationFrame(resolve); }); })');
  const seededProbe = await readProbe(cdp);
  const seededMetrics = await chromeMetrics(cdp);
  const seedState = await cdp.evaluate('window.__probe.state()');
  // Reset the stream-only counters, keeping the seed DOM in place.
  await cdp.evaluate('window.__faktorPerf.messages = 0; window.__faktorPerf.snapshotMessages = 0; window.__faktorPerf.layoutReads = 0; window.__faktorPerf.layoutWrites = 0; window.__faktorPerf.queries = 0; window.__faktorPerf.mutations = 0; window.__faktorPerf.frames = 0; window.__faktorPerf.renderCounts = {}; window.__faktorPerf.values = {}; window.__faktorPerf.series = {};');
  const started = Date.now();
  const config = await cdp.evaluate(
    `window.__probe.begin(${pinned ? 'true' : 'false'}, ${select ? 'true' : 'false'})`,
  );
  for (let attempt = 0; attempt < 600; attempt += 1) {
    if (await cdp.evaluate('window.__probeDone === true')) {
      break;
    }
    await sleep(10);
  }
  await sleep(250);
  const elapsed = Date.now() - started;
  const streamProbe = await readProbe(cdp);
  const streamMetrics = await chromeMetrics(cdp);
  const state = await cdp.evaluate('window.__probe.state()');
  return {
    scenario: select ? 'selection' : pinned ? 'pinned' : 'unpinned',
    config: { ...config, tokensPerDelta: 20, deltasPerSecond: 200 },
    seed: {
      passed: {
        messages: seededProbe.messages,
        renders: seededProbe.renderCounts.snapshot || 0,
        nodes: seedState.nodes,
        children: seedState.children,
        layoutReads: seededProbe.layoutReads,
        layoutWrites: seededProbe.layoutWrites,
        queries: seededProbe.queries,
        mutations: seededProbe.mutations,
      },
      chrome: {
        LayoutCount: seededMetrics.LayoutCount,
        RecalcStyleCount: seededMetrics.RecalcStyleCount,
        LayoutDuration: seededMetrics.LayoutDuration,
        ScriptDuration: seededMetrics.ScriptDuration,
      },
    },
    stream: {
      elapsedMs: elapsed,
      passed: delta(streamProbe, empty),
      renderCounts: streamProbe.renderCounts,
      values: streamProbe.values || null,
      series: streamProbe.series || null,
      final: state,
      chromeDelta: delta(streamMetrics, seededMetrics),
    },
  };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const chrome = findChrome();
  if (!chrome) {
    throw new Error('no chrome/chromium binary found');
  }
  mkdirSync(OUT_DIR, { recursive: true });
  const html = buildHarnessHtml(false, false, false, 'dark', {
    skipSnapshot: true,
    headScript: PROBE_SCRIPT,
  });
  const dir = join(OUT_DIR, `.stream-probe-${process.pid}`);
  mkdirSync(dir, { recursive: true });
  const pagePath = join(dir, 'index.html');
  writeFileSync(pagePath, html);
  const url = pathToFileURL(pagePath).href;
  const { child, pageWs } = await launchChrome(chrome);
  const cdp = await connect(pageWs);
  try {
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    await cdp.send('Performance.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width: 480,
      height: 900,
      deviceScaleFactor: 1,
      mobile: false,
    });
    const pinned = await runScenario(cdp, url, true);
    const unpinned = await runScenario(cdp, url, false);
    const selection = await runScenario(cdp, url, true, true);
    const result = {
      schema: 'faktor-vscode-stream-probe/v1',
      capturedAt: new Date().toISOString(),
      label: args.label,
      transcriptMessages: 500,
      scenarios: [pinned, unpinned, selection],
    };
    const file = join(OUT_DIR, `stream-probe-${args.label}.json`);
    writeFileSync(file, `${JSON.stringify(result, null, 2)}\n`);
    console.log(`stream probe written: ${file}`);
    for (const scenario of result.scenarios) {
      const stream = scenario.stream;
      console.log(
        `${scenario.scenario}: messages=${stream.passed.messages} renders=${stream.renderCounts.snapshot || 0} ` +
          `nodes=${stream.final.nodes} layoutReads=${stream.passed.layoutReads} ` +
          `queries=${stream.passed.queries} mutations=${stream.passed.mutations} ` +
          `chromeLayouts=${stream.chromeDelta.LayoutCount} chromeRecalc=${stream.chromeDelta.RecalcStyleCount}`,
      );
    }
    if (args.compare) {
      const beforePath = join(OUT_DIR, `stream-probe-${args.compare}.json`);
      const before = JSON.parse(readFileSync(beforePath, 'utf8'));
      console.log(`comparison against ${beforePath}:`);
      for (const scenario of result.scenarios) {
        const prior = before.scenarios.find((entry) => entry.scenario === scenario.scenario);
        if (!prior) {
          continue;
        }
        const keys = ['messages', 'renders', 'nodes', 'layoutReads', 'queries', 'mutations'];
        const parts = keys.map((key) => {
          const afterValue =
            key === 'renders'
              ? scenario.stream.renderCounts.snapshot || 0
              : key === 'nodes'
                ? scenario.stream.final.nodes
                : scenario.stream.passed[key];
          const beforeValue =
            key === 'renders'
              ? prior.stream.renderCounts.snapshot || 0
              : key === 'nodes'
                ? prior.stream.final.nodes
                : prior.stream.passed[key];
          return `${key} ${beforeValue}->${afterValue}`;
        });
        console.log(`${scenario.scenario}: ${parts.join(' ')}`);
      }
    }
  } finally {
    cdp.ws.close();
    child.kill('SIGKILL');
    rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(`stream-probe error: ${error && error.stack ? error.stack : error}`);
  process.exitCode = 1;
});
