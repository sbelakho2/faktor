#!/usr/bin/env node
// Faktor VS Code webview headless-Chrome verification harness.
//
// Renders the REAL media/chat.css + media/chat.js + media/composer-state.js +
// media/board-state.js (and the REAL markup template from src/webview.ts) in
// headless Chrome with a stubbed VS Code dark theme, then:
//
//   --screenshots --prefix NAME   capture full-page PNGs per width under
//                                 scripts/baselines/screenshots/
//   --check                       measure every visible interactive control
//                                 (>= 24x24 CSS px; primary buttons >= 28x28)
//                                 and prove the keyboard focus ring exists and
//                                 that pointer focus is suppressed. Exits
//                                 nonzero on any failure.
//
// It drives Chrome over the DevTools protocol with Node's built-in WebSocket;
// no npm dependencies. Usage is documented in scripts/render-webview.md.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { mkdtempSync } from 'node:fs';

// Node's global WebSocket is flag-gated before Node 22. Re-exec once with
// the flag so the harness works on every supported Node without asking the
// caller to remember it.
if (typeof WebSocket === 'undefined' && process.env.FAKTOR_RENDER_REEXEC !== '1') {
  const result = spawnSync(
    process.execPath,
    ['--experimental-websocket', ...process.argv.slice(1)],
    { stdio: 'inherit', env: { ...process.env, FAKTOR_RENDER_REEXEC: '1' } },
  );
  process.exit(result.status ?? 1);
}

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const MEDIA = join(ROOT, 'media');
const SHOT_DIR = join(HERE, 'baselines', 'screenshots');
const WIDTHS = [240, 320, 480, 800];

// The VS Code dark-theme variables the panel consumes. Values are the stock
// "Dark Modern" / "Dark+" values so screenshots look like the real renderer.
const THEME_CSS = `:root {
  --vscode-font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
  --vscode-font-size: 13px;
  --vscode-editor-font-family: "SF Mono", Menlo, Consolas, monospace;
  --vscode-foreground: #cccccc;
  --vscode-descriptionForeground: #9d9d9d;
  --vscode-disabledForeground: #8c8c8c;
  --vscode-sideBar-background: #252526;
  --vscode-panel-border: #3c3c3c;
  --vscode-button-foreground: #ffffff;
  --vscode-button-background: #0e639c;
  --vscode-button-hoverBackground: #1177bb;
  --vscode-badge-background: #4d4d4d;
  --vscode-badge-foreground: #ffffff;
  --vscode-testing-iconPassed: #73c991;
  --vscode-testing-iconFailed: #f14c4c;
  --vscode-editorWarning-foreground: #cca700;
  --vscode-input-background: #3c3c3c;
  --vscode-input-foreground: #cccccc;
  --vscode-input-border: #3c3c3c;
  --vscode-focusBorder: #007fd4;
  --vscode-textLink-foreground: #3794ff;
  --vscode-textLink-activeForeground: #4daafc;
  --vscode-textCodeBlock-background: #0a0a0a;
  --vscode-textBlockQuote-background: #2b2b2b;
  --vscode-editorWidget-background: #252526;
  --vscode-toolbar-hoverBackground: rgba(90, 93, 94, 0.31);
  --vscode-widget-border: #3c3c3c;
  --vscode-widget-shadow: rgba(0, 0, 0, 0.36);
  --vscode-button-secondaryBackground: #3a3d41;
  --vscode-button-secondaryForeground: #cccccc;
  --vscode-button-secondaryHoverBackground: #45494e;
  --vscode-inputValidation-infoBackground: #063b49;
  --vscode-inputValidation-infoBorder: #007acc;
  --vscode-inputValidation-errorBackground: #5a1d1d;
  --vscode-inputValidation-errorBorder: #be1100;
}`;

function chromeCandidates() {
  const names = ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'];
  const found = [];
  if (process.env.FAKTOR_CHROME) {
    found.push(process.env.FAKTOR_CHROME);
  }
  for (const dir of (process.env.PATH || '').split(':')) {
    if (!dir) {
      continue;
    }
    for (const name of names) {
      const candidate = join(dir, name);
      if (existsSync(candidate)) {
        found.push(candidate);
      }
    }
  }
  return found;
}

export function findChrome() {
  for (const candidate of chromeCandidates()) {
    const probe = spawnSync(candidate, ['--version'], { encoding: 'utf8', timeout: 15000 });
    if (probe.status === 0) {
      return candidate;
    }
  }
  return null;
}

/** The deterministic, self-contained snapshot the harness renders. */
function harnessSnapshot(empty = false) {
  const snapshot = {
    daemon: 'running',
    daemonDetail: 'http://127.0.0.1:7337',
    baseUrl: 'http://127.0.0.1:7337',
    session: {
      id: '7',
      title: 'Audit session',
      provider: 'openai',
      model: 'gpt-5',
      state: 'running',
    },
    machineState: 'running',
    machineLabel: 'Running · ready',
    sessions: [],
    runs: [],
    activeRunId: 'r1',
    agents: [
      {
        agentId: 'r1',
        kind: 'self',
        runId: 'r1',
        sessionId: 7,
        worktreeId: 1,
        goal: 'Ship the UX audit fixes',
        state: 'Running',
        model: null,
        provider: null,
        budget: null,
        ownership: 'self',
        capabilities: [],
        progress: null,
        result: null,
        blockers: [],
        presentation: 'foreground',
        pixel: null,
      },
      {
        agentId: 'c1',
        kind: 'child',
        runId: 'r1',
        sessionId: 8,
        worktreeId: 2,
        goal: 'Implement the target-size fixes',
        state: 'Running',
        model: 'gpt-5',
        provider: 'openai',
        budget: 100000,
        ownership: 'orchestrator',
        capabilities: ['ReadWorkspace', 'EditWorkspace'],
        progress: { phase: 'work' },
        result: null,
        itemId: 'main',
        itemKind: 'Implementation',
        blockers: [],
        presentation: 'foreground',
        pixel: null,
      },
      {
        agentId: 'c2',
        kind: 'child',
        runId: 'r1',
        sessionId: 9,
        worktreeId: 3,
        goal: 'Verify the responsive breakpoints',
        state: 'Waiting',
        model: 'gpt-5',
        provider: 'openai',
        budget: 50000,
        ownership: 'orchestrator',
        capabilities: ['ReadWorkspace'],
        progress: null,
        result: null,
        blockers: [],
        presentation: 'background',
        pixel: null,
      },
    ],
    task: {
      state: 'running',
      goal: 'Close the UX audit interaction defects',
      completed: ['Reconcile the audit findings', 'Fix target sizes and focus'],
      open: ['Re-run the render checks'],
      testsRun: ['selftest', 'render-webview'],
      testsFailed: ['selftest'],
      phase: 'verify',
      completion: {
        source: 'daemon',
        steps: [
          { step: 'commit', status: 'succeeded' },
          { step: 'push', status: 'succeeded', detail: 'origin/main' },
          { step: 'pr', status: 'pending' },
        ],
        reason: null,
      },
    },
    verification: null,
    usage: null,
    cockpit: { state: 'ok' },
    cockpitSections: [
      {
        key: 'acceptance',
        title: 'Acceptance criteria',
        present: true,
        lines: [],
        evidence: [],
        actions: [],
        criteria: [
          {
            verdict: 'pass',
            criterionKey: 'target size minimum',
            requirement: 'required',
            origin: 'user',
            binding: 'binding-digest-1',
            bindingSource: 'daemon',
            bindingReference: 'check:css',
          },
          {
            verdict: 'unavailable',
            criterionKey: 'focus ring renderer check',
            requirement: 'required',
            origin: 'audit',
            binding: 'unavailable',
            bindingSource: 'none',
            bindingReference: null,
          },
        ],
      },
      {
        key: 'tournament',
        title: 'Tournament',
        present: true,
        lines: ['candidate child-0: [pass] tests'],
        evidence: [],
        actions: [
          { key: 'decide', label: 'Decide', enabled: true },
          { key: 'abort', label: 'Abort', enabled: false },
        ],
      },
      {
        key: 'evidence',
        title: 'Evidence',
        present: true,
        lines: ['terminal transcript'],
        evidence: [{ id: 41 }],
        actions: [],
      },
      {
        key: 'usage',
        title: 'Usage',
        present: true,
        lines: ['totals: 130 tokens', '[EXCEEDED] spend cap'],
        evidence: [],
        actions: [{ key: 'usage-next', label: 'Load more usage', enabled: true }],
      },
    ],
    tournament: { id: 't-1' },
    transcript: [
      {
        id: 'm-1',
        role: 'user',
        seq: 1,
        createdMs: 1,
        text: 'Please fix the accessibility findings.',
        reasoning: '',
        summary: '',
        tools: [],
      },
      {
        id: 'm-2',
        role: 'assistant',
        seq: 2,
        createdMs: 2,
        text: 'I measured every control and added the focus ring across the composer and transcript.',
        reasoning: 'The audit requires 24px minimum targets and a visible keyboard focus ring.',
        summary: 'Targets and focus-visible implemented; one selftest is still failing.',
        tools: [
          {
            name: 'bash',
            state: 'completed',
            excerpt: 'render check: 41 controls pass',
            exitCode: 0,
            artifact: null,
          },
          {
            name: 'edit_file',
            state: 'completed',
            excerpt:
              '@@ apps/vscode/media/chat.css @@\n+  min-height: 28px;\n+  outline-offset: 1px;\n+  border-radius: 6px;\n-  border-radius: 2px;\n-  padding: 4px 10px;',
            exitCode: null,
            artifact: null,
          },
          {
            name: 'cargo test',
            state: 'completed',
            excerpt: 'FAILED apps/vscode/scripts/selftest.mjs::target-sizes',
            exitCode: 1,
            artifact: 'evidence:41',
          },
          {
            name: 'verify_changes',
            state: 'completed',
            excerpt: 'criteria 2/2 passed · landed == verified',
            exitCode: 0,
            artifact: null,
          },
        ],
      },
    ],
    board: {
      available: true,
      source: 'native',
      revision: 3,
      unread: 1,
      reason: null,
      posts: [
        {
          id: '3',
          author: 'child:8',
          subject: 'handoff',
          body: 'main step ready',
          refs: ['evidence:41'],
          revision: 3,
          createdMs: 1700,
        },
      ],
    },
    streamStatus: 'open',
    lastError: null,
    busy: false,
  };
  if (empty) {
    // The onboarding/empty-state fixture: no conversation, no run, no
    // agents — the welcome state is the only content in the transcript slot.
    snapshot.transcript = [];
    snapshot.task = null;
    snapshot.agents = [];
    snapshot.cockpit = null;
    snapshot.cockpitSections = [];
    snapshot.tournament = null;
    snapshot.verification = null;
    snapshot.usage = null;
    snapshot.usagePanel = null;
    snapshot.indexCoverage = null;
    snapshot.runs = [];
    snapshot.board = {
      available: false,
      source: 'none',
      revision: null,
      unread: null,
      reason: 'no run-family board yet',
      posts: [],
    };
  }
  return snapshot;
}

/** Strip the CSP + nonces, point the template at real files, inject stubs. */
export function buildHarnessHtml(empty = false, attachments = false) {
  const webviewSource = readFileSync(join(ROOT, 'src', 'webview.ts'), 'utf8');
  const start = webviewSource.indexOf('<!DOCTYPE html>');
  const end = webviewSource.indexOf('</html>', start) + '</html>'.length;
  if (start < 0 || end < start) {
    throw new Error('src/webview.ts no longer contains the webview HTML template');
  }
  let html = webviewSource.slice(start, end);
  html = html.replace(/<meta http-equiv="Content-Security-Policy"[^>]*>\s*/, '');
  html = html.replace(/\snonce="\$\{nonce\}"/g, '');
  const fileUrl = (path) => pathToFileURL(path).href;
  for (const [placeholder, path] of [
    ['${styleUri}', join(MEDIA, 'chat.css')],
    ['${boardUri}', join(MEDIA, 'board-state.js')],
    ['${composerUri}', join(MEDIA, 'composer-state.js')],
    ['${scriptUri}', join(MEDIA, 'chat.js')],
  ]) {
    if (!html.includes(placeholder)) {
      throw new Error(`src/webview.ts lost the ${placeholder} placeholder`);
    }
    html = html.replace(placeholder, fileUrl(path));
  }
  const theme = `<style id="faktor-harness-theme">${THEME_CSS}</style>`;
  const apiStub = `<script>window.__posted = []; window.acquireVsCodeApi = function () { return { postMessage: function (message) { window.__posted.push(message); } }; };</script>`;
  const snapshotJson = JSON.stringify(harnessSnapshot(empty)).replace(/</g, '\\u003c');
  // The attachment fixture exercises the compact attachment cards (ready and
  // refused) exactly the way the host delivers them.
  const attachmentsScript = attachments
    ? `
    window.postMessage({ type: 'attachments', items: [
      { id: 'att-1', filename: 'chat.css', mime: 'text/css', bytes: 21477 },
      { id: 'att-2', filename: 'screen.png', mime: 'image/png', bytes: 284112, isImage: true },
      { id: 'att-3', filename: 'archive.zip', mime: 'application/zip', bytes: 9999999, refusal: 'application/zip is outside the advertised attachment mime types' }
    ] }, '*');`
    : '';
  const snapshotScript = `<script>
    window.postMessage({ type: 'snapshot', snapshot: ${snapshotJson} }, '*');${attachmentsScript}
    setTimeout(function () { window.__ready = true; }, 120);
  </script>`;
  html = html.replace('</head>', `${theme}</head>`);
  html = html.replace('<body>', `<body>${apiStub}`);
  html = html.replace('</body>', `${snapshotScript}</body>`);
  return html;
}

// ------------------------------------------------------------------ CDP

class Cdp {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.closed = false;
    ws.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.id !== undefined) {
        const entry = this.pending.get(message.id);
        if (entry) {
          this.pending.delete(message.id);
          if (message.error) {
            entry.reject(new Error(`${entry.method}: ${message.error.message}`));
          } else {
            entry.resolve(message.result);
          }
        }
        return;
      }
      for (const listener of this.listeners.get(message.method) || []) {
        listener(message.params);
      }
    });
    ws.addEventListener('close', () => {
      this.closed = true;
      for (const entry of this.pending.values()) {
        entry.reject(new Error('CDP connection closed'));
      }
      this.pending.clear();
    });
  }

  send(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, method });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  waitFor(method, timeoutMs = 15000) {
    return new Promise((resolve, reject) => {
      const listeners = this.listeners.get(method) || [];
      const timer = setTimeout(() => {
        this.listeners.set(method, listeners.filter((entry) => entry !== onEvent));
        reject(new Error(`timed out waiting for ${method}`));
      }, timeoutMs);
      const onEvent = (params) => {
        clearTimeout(timer);
        this.listeners.set(method, listeners.filter((entry) => entry !== onEvent));
        resolve(params);
      };
      listeners.push(onEvent);
      this.listeners.set(method, listeners);
    });
  }

  async evaluate(expression) {
    const result = await this.send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
    });
    if (result.exceptionDetails) {
      throw new Error(`evaluate failed: ${result.exceptionDetails.text}`);
    }
    return result.result.value;
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function connect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', () => reject(new Error('websocket connect failed')), {
      once: true,
    });
  });
  return new Cdp(ws);
}

async function launchChrome(chrome) {
  const profile = mkdtempSync(join(tmpdir(), 'faktor-render-profile-'));
  const child = spawn(
    chrome,
    [
      '--headless=new',
      '--remote-debugging-port=0',
      `--user-data-dir=${profile}`,
      '--no-first-run',
      '--no-default-browser-check',
      '--disable-gpu',
      '--hide-scrollbars',
      '--force-device-scale-factor=1',
      '--allow-file-access-from-files',
      '--disable-extensions',
      'about:blank',
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  const wsUrl = await new Promise((resolve, reject) => {
    let stderr = '';
    const timer = setTimeout(() => reject(new Error(`chrome did not report DevTools: ${stderr}`)), 20000);
    child.stderr.on('data', (chunk) => {
      stderr += chunk.toString();
      const match = /DevTools listening on (ws:\/\/\S+)/.exec(stderr);
      if (match) {
        clearTimeout(timer);
        resolve(match[1]);
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`chrome exited early (${code}): ${stderr}`));
    });
  });
  const port = new URL(wsUrl).port;
  const list = await fetch(`http://127.0.0.1:${port}/json/list`).then((response) => response.json());
  const page = list.find((target) => target.type === 'page');
  if (!page) {
    throw new Error('no page target in chrome');
  }
  return { child, profile, pageWs: page.webSocketDebuggerUrl };
}

const MEASURE_SCRIPT = `(() => {
  const selector = 'button, [role="button"], input, select, textarea, a[href], summary, [tabindex]:not([tabindex="-1"])';
  const out = [];
  const boxes = [];
  for (const el of document.querySelectorAll(selector)) {
    const visible = el.getClientRects().length > 0 && !el.closest('[hidden]');
    if (!visible) continue;
    const measured = el.type === 'checkbox' || el.type === 'radio'
      ? (el.closest('label') || el)
      : el;
    const rect = measured.getBoundingClientRect();
    let name = el.id || el.tagName.toLowerCase();
    if (el.type === 'checkbox' || el.type === 'radio') name = 'label-for-' + name;
    const inControls = el.closest('.agent-controls') !== null;
    const secondary = inControls || el.classList.contains('attachment-remove');
    out.push({
      name: name + (el.id ? '' : (el.className ? '.' + String(el.className).split(' ')[0] : '')),
      tag: el.tagName,
      w: Math.round(rect.width * 100) / 100,
      h: Math.round(rect.height * 100) / 100,
      primary: el.tagName === 'BUTTON' && !secondary,
    });
    boxes.push({ name, x: rect.x, y: rect.y, w: rect.width, h: rect.height });
  }
  return {
    controls: out,
    boxes,
    innerWidth: window.innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
  };
})()`;

async function loadPage(cdp, url, width, height = 1000) {
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Emulation.setDeviceMetricsOverride', {
    width,
    height,
    deviceScaleFactor: 1,
    mobile: false,
  });
  const loaded = cdp.waitFor('Page.loadEventFired');
  await cdp.send('Page.navigate', { url });
  await loaded;
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const ready = await cdp.evaluate('window.__ready === true');
    if (ready) {
      break;
    }
    await sleep(50);
  }
  await cdp.evaluate('new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))');
}

async function capture(cdp, file, width) {
  const metrics = await cdp.send('Page.getLayoutMetrics');
  const height = Math.min(Math.ceil(metrics.cssContentSize.height), 6000);
  const shot = await cdp.send('Page.captureScreenshot', {
    format: 'png',
    captureBeyondViewport: true,
    clip: { x: 0, y: 0, width, height, scale: 1 },
  });
  writeFileSync(file, Buffer.from(shot.data, 'base64'));
  return height;
}

const MAIN_MIN = 28;
const OTHER_MIN = 24;

function parseArgs(argv) {
  const args = { check: false, screenshots: false, empty: false, attachments: false, prefix: 'shot', out: SHOT_DIR };
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--check') args.check = true;
    else if (argv[i] === '--screenshots') args.screenshots = true;
    else if (argv[i] === '--empty') args.empty = true;
    else if (argv[i] === '--attachments') args.attachments = true;
    else if (argv[i] === '--prefix') args.prefix = argv[++i];
    else if (argv[i] === '--out') args.out = argv[++i];
  }
  return args;
}

export async function runHarness(args) {
  const chrome = findChrome();
  if (!chrome) {
    throw new Error('no chrome/chromium binary found on PATH');
  }
  const html = buildHarnessHtml(args.empty === true, args.attachments === true);
  const pageDir = mkdtempSync(join(tmpdir(), 'faktor-render-page-'));
  const pagePath = join(pageDir, 'index.html');
  writeFileSync(pagePath, html);
  const pageUrl = pathToFileURL(pagePath).href;

  const { child, pageWs } = await launchChrome(chrome);
  const cdp = await connect(pageWs);
  const failures = [];
  const measured = new Map();
  try {
    if (args.check) {
      for (const width of WIDTHS) {
        await loadPage(cdp, pageUrl, width);
        const result = await cdp.evaluate(MEASURE_SCRIPT);
        const overflows = result.scrollWidth > result.innerWidth + 1;
        if (overflows) {
          failures.push(`${width}px: horizontal overflow (scrollWidth ${result.scrollWidth} > innerWidth ${result.innerWidth})`);
        }
        for (const control of result.controls) {
          measured.set(control.name, control);
          const min = control.primary ? MAIN_MIN : OTHER_MIN;
          if (control.w < min - 0.51 || control.h < min - 0.51) {
            failures.push(
              `${width}px: ${control.name} is ${control.w}x${control.h} (needs >= ${min}x${min})`,
            );
          }
        }
      }
      // Keyboard focus ring: fresh load, press Tab, inspect the focused node.
      await loadPage(cdp, pageUrl, 480);
      await cdp.send('Input.dispatchKeyEvent', {
        type: 'rawKeyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9,
      });
      await cdp.send('Input.dispatchKeyEvent', {
        type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9,
      });
      const focus = await cdp.evaluate(`(() => {
        const el = document.activeElement;
        const cs = getComputedStyle(el);
        return {
          tag: el.tagName, id: el.id, cls: String(el.className),
          outlineStyle: cs.outlineStyle, outlineWidth: cs.outlineWidth,
          outlineColor: cs.outlineColor, outlineOffset: cs.outlineOffset,
        };
      })()`);
      if (focus.outlineStyle !== 'solid' || parseFloat(focus.outlineWidth) < 2) {
        failures.push(
          `keyboard focus ring missing on ${focus.tag}#${focus.id}: outline ${focus.outlineStyle} ${focus.outlineWidth}`,
        );
      }
      // Pointer focus must NOT show the ring (suppression).
      await loadPage(cdp, pageUrl, 480);
      const refreshBox = await cdp.evaluate(`(() => {
        const r = document.getElementById('btn-refresh').getBoundingClientRect();
        return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
      })()`);
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: refreshBox.x, y: refreshBox.y, button: 'left', clickCount: 1 });
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: refreshBox.x, y: refreshBox.y, button: 'left', clickCount: 1 });
      const pointer = await cdp.evaluate(`(() => {
        const el = document.activeElement;
        const cs = getComputedStyle(el);
        return { id: el.id, outlineStyle: cs.outlineStyle };
      })()`);
      if (pointer.outlineStyle !== 'none') {
        failures.push(`pointer click on #btn-refresh left outline ${pointer.outlineStyle}`);
      }
      console.log('RENDER CHECK measured controls (largest width):');
      for (const control of measured.values()) {
        console.log(`  ${control.primary ? 'primary' : 'other  '} ${control.name}: ${control.w}x${control.h}`);
      }
      console.log(`RENDER CHECK focus: ${focus.tag}#${focus.id} outline ${focus.outlineWidth} ${focus.outlineStyle} ${focus.outlineColor} offset ${focus.outlineOffset}`);
      console.log(`RENDER CHECK pointer suppression: outline ${pointer.outlineStyle}`);
      if (failures.length > 0) {
        console.log('RENDER CHECK FAIL');
        for (const failure of failures) {
          console.log(`  - ${failure}`);
        }
        return { status: 1, failures };
      }
      console.log('RENDER CHECK OK');
      return { status: 0, failures };
    }

    if (args.screenshots) {
      mkdirSync(args.out, { recursive: true });
      for (const width of WIDTHS) {
        await loadPage(cdp, pageUrl, width);
        const file = join(args.out, `${args.prefix}-${width}.png`);
        const height = await capture(cdp, file, width);
        console.log(`screenshot ${file} (${width}x${height})`);
      }
      // Keyboard-focus proof at 480px: tab to the send button, then capture.
      await loadPage(cdp, pageUrl, 480);
      for (let i = 0; i < 40; i += 1) {
        const active = await cdp.evaluate('document.activeElement ? document.activeElement.id : ""');
        if (active === 'btn-send') {
          break;
        }
        await cdp.send('Input.dispatchKeyEvent', {
          type: 'rawKeyDown', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9,
        });
        await cdp.send('Input.dispatchKeyEvent', {
          type: 'keyUp', key: 'Tab', code: 'Tab', windowsVirtualKeyCode: 9, nativeVirtualKeyCode: 9,
        });
      }
      const focus = await cdp.evaluate(`(() => {
        const el = document.activeElement;
        const cs = getComputedStyle(el);
        return { id: el.id, outlineStyle: cs.outlineStyle, outlineWidth: cs.outlineWidth, outlineColor: cs.outlineColor, outlineOffset: cs.outlineOffset };
      })()`);
      const focusFile = join(args.out, `${args.prefix}-focus-480.png`);
      await capture(cdp, focusFile, 480);
      console.log(
        `screenshot ${focusFile} (focused #${focus.id}, outline ${focus.outlineWidth} ${focus.outlineStyle} ${focus.outlineColor})`,
      );
      return { status: 0, failures: [] };
    }

    throw new Error('nothing to do: pass --check or --screenshots');
  } finally {
    cdp.ws.close();
    child.kill('SIGKILL');
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const result = await runHarness(args);
  process.exitCode = result.status;
}

const isMain = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isMain) {
  main().catch((error) => {
    console.error(`render-webview error: ${error.message}`);
    process.exitCode = 1;
  });
}
