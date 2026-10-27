#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile,read-only-artifacts
// Faktor VS Code webview headless-Chrome verification harness.
//
// Renders the REAL media/chat.css + media/chat.js + media/composer-state.js +
// media/board-state.js (and the REAL markup template from src/webview.ts) in
// headless Chrome with a stubbed VS Code theme (dark and light), then:
//
//   --screenshots --prefix NAME   capture full-page PNGs per width under
//                                 scripts/baselines/screenshots/
//   --check                       measure every visible interactive control
//                                 (>= 24x24 CSS px; primary buttons >= 28x28)
//                                 and prove the keyboard focus ring exists and
//                                 that pointer focus is suppressed. The full
//                                 matrix runs under BOTH the dark and the
//                                 light theme at 240/320/480/800, on the
//                                 populated and the empty/preflight surface;
//                                 a DPR leg (deviceScaleFactor 1.5 and 2.0)
//                                 and the VAL-ZOOM reflow leg (IDE UI scale
//                                 150%/200%, modelled as a reduced CSS-pixel
//                                 budget) extend the same fail-closed
//                                 invariants to HiDPI rendering and IDE UI
//                                 scale. Exits nonzero on any failure.
//   --theme dark|light|both       theme leg filter for --check (default both;
//                                 screenshots stay dark unless light is
//                                 selected explicitly).
//
// It drives Chrome over the DevTools protocol with Node's built-in WebSocket;
// no npm dependencies. Usage is documented in scripts/render-webview.md.

import { spawn, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { mkdtempSync } from 'node:fs';
import { projectUx } from '../src/uxState.ts';
import { renderEdition8AppMarkup } from '../src/edition8/html.ts';

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
  --vscode-editor-background: #1e1e1e;
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

// The VS Code light-theme counterpart over the same variable set: stock
// "Light Modern" values where that theme defines the token, otherwise the
// light fallback the theme registry resolves for a light theme
// (disabledForeground, widget.shadow, toolbar.hoverBackground, editorWarning,
// the testing icons and the input-validation colors).
const LIGHT_THEME_CSS = `:root {
  --vscode-font-family: system-ui, -apple-system, "Segoe UI", sans-serif;
  --vscode-font-size: 13px;
  --vscode-editor-background: #ffffff;
  --vscode-editor-font-family: "SF Mono", Menlo, Consolas, monospace;
  --vscode-foreground: #3b3b3b;
  --vscode-descriptionForeground: #3b3b3b;
  --vscode-disabledForeground: #61616180;
  --vscode-sideBar-background: #f8f8f8;
  --vscode-panel-border: #e5e5e5;
  --vscode-button-foreground: #ffffff;
  --vscode-button-background: #005fb8;
  --vscode-button-hoverBackground: #0258a8;
  --vscode-badge-background: #cccccc;
  --vscode-badge-foreground: #3b3b3b;
  --vscode-testing-iconPassed: #73c991;
  --vscode-testing-iconFailed: #f14c4c;
  --vscode-editorWarning-foreground: #bf8803;
  --vscode-input-background: #ffffff;
  --vscode-input-foreground: #3b3b3b;
  --vscode-input-border: #cecece;
  --vscode-focusBorder: #005fb8;
  --vscode-textLink-foreground: #005fb8;
  --vscode-textLink-activeForeground: #005fb8;
  --vscode-textCodeBlock-background: #f8f8f8;
  --vscode-textBlockQuote-background: #f8f8f8;
  --vscode-editorWidget-background: #f8f8f8;
  --vscode-toolbar-hoverBackground: #b8b8b850;
  --vscode-widget-border: #e5e5e5;
  --vscode-widget-shadow: rgba(0, 0, 0, 0.16);
  --vscode-button-secondaryBackground: #e5e5e5;
  --vscode-button-secondaryForeground: #3b3b3b;
  --vscode-button-secondaryHoverBackground: #cccccc;
  --vscode-inputValidation-infoBackground: #d6ecf2;
  --vscode-inputValidation-infoBorder: #007acc;
  --vscode-inputValidation-errorBackground: #f2dede;
  --vscode-inputValidation-errorBorder: #be1100;
}`;

export const THEMES = { dark: THEME_CSS, light: LIGHT_THEME_CSS };
// DPR leg: deviceScaleFactor values emulated on top of every width.
const DPRS = [1.5, 2.0];
// VAL-ZOOM leg: IDE UI scale values. A zoomed webview sees fewer CSS pixels
// for the same device width (viewport width / zoom), so the zoom is modelled
// as a reduced CSS-pixel budget, floored at 120px.
const ZOOMS = [1.5, 2.0];
const ZOOM_MIN_WIDTH = 120;

function chromeCandidates() {
  const names = ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser'];
  const found = [];
  if (process.env.FAKTOR_CHROME) {
    found.push(process.env.FAKTOR_CHROME);
  }
  // Installed browser apps are not on PATH in the VS Code/macOS/Windows
  // environments this harness runs in; probe the platform app locations.
  found.push(
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
    join(process.env.HOME || '', 'Applications/Google Chrome.app/Contents/MacOS/Google Chrome'),
    'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
    'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
  );
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
function harnessSnapshot(empty = false, permissions = false, problem = false) {
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
    machineState: 'ready_for_next_turn',
    machineLabel: 'Ready for next turn',
    sessions: [],
    // Adversarial raw states: the chat body must never render any of them.
    runs: [
      {
        taskId: '3',
        runId: 'r1',
        mode: 'in_session',
        state: 'Verifying',
        goal: 'Close the UX audit interaction defects',
        model: null,
      },
    ],
    proofTaskState: 'verifying',
    activeRunId: 'r1',
    permissions: permissions
      ? [
          {
            id: '41',
            sessionId: '7',
            capability: 'shell',
            detail: '{"tool":"bash","command":"cargo test --workspace"}',
          },
        ]
      : [],
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
      // The canned engineering contract + uncertainty state: a high risk, an
      // unknown risk and an unsatisfied obligation force the "Needs
      // disclosure" block to lead the run details, and every contract row
      // exercises the provenance/severity/state/confidence chips.
      {
        key: 'disclosure',
        title: 'Needs disclosure',
        present: true,
        lines: [
          'risk:high · race conditions',
          'risk:unknown · legacy client incompatibility',
          'inferred · replayed refresh tokens fail',
        ],
        evidence: [],
        actions: [],
      },
      {
        key: 'contract',
        title: 'Contract',
        present: true,
        lines: [],
        evidence: [],
        actions: [],
        contract: {
          constraints: [
            { statement: 'existing sessions remain valid', origin: 'user' },
            { statement: 'existing API clients cannot break', origin: 'native' },
          ],
          invariants: [
            { statement: 'revoked sessions cannot authenticate', origin: 'example.graph' },
            { statement: 'refresh tokens cannot be replayed', origin: 'policy' },
          ],
          risks: [
            {
              description: 'race conditions',
              severity: 'high',
              origin: 'heuristic',
              disclosure: 'risk:high',
            },
            {
              description: 'migration corruption',
              severity: 'medium',
              origin: 'native',
              disclosure: null,
            },
            {
              description: 'legacy client incompatibility',
              severity: 'unknown',
              origin: 'policy',
              disclosure: 'risk:unknown',
            },
          ],
          obligations: [
            {
              claim: 'concurrent refresh produces one valid result',
              evidence: 'test execution',
              state: 'supported',
              stateText: 'supported · 91%',
              origin: 'user',
              evidenceRefs: ['test execution · cargo test · refresh suite'],
            },
            {
              claim: 'replayed refresh tokens fail',
              evidence: 'contract check',
              state: 'inferred',
              stateText: 'inferred · 50%',
              origin: 'native',
              evidenceRefs: [],
            },
            {
              claim: 'legacy sessions survive the migration',
              evidence: 'review',
              state: 'uncertain',
              stateText: 'uncertain',
              origin: 'example.graph',
              evidenceRefs: [],
            },
          ],
          droppedRows: 0,
          malformed: null,
        },
      },
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
    // The preflight/readiness state the host publishes (I06): the stopped row
    // renders in the Work empty state and the Diagnostics mirror.
    preflight: {
      state: 'stopped',
      label: 'Faktor is stopped',
      copy: 'Start the service when you are ready.',
      actions: [{ key: 'startService', label: 'Start service' }],
      executable: '/opt/faktor/bin/faktor-cli',
      trust: 'verified',
      trustLabel: 'Matches recorded checksum',
      trustDetail: 'sha256 matches the checksum record /opt/faktor/bin/faktor-cli.sha256',
      readiness: 'idle',
      readinessDetail: null,
      failureDetail: null,
    },
  };
  if (problem) {
    // The adversarial problem fixture: an authority refusal, a blocked
    // stream and the raw last error all describe the same failure family.
    // The redesigned chat body must merge them into ONE inline attention
    // card; the legacy surface stacked three separate panels.
    snapshot.streamStatus = 'protocol_blocked';
    snapshot.serviceAuthority = {
      kind: 'outside_trusted_install',
      label: 'Faktor refused an untrusted executable',
      copy: 'The configured service executable is outside the trusted install, so Faktor did not start it.',
      actions: [
        { key: 'locateDaemon', label: 'Locate service…' },
        { key: 'openDiagnostics', label: 'Open Diagnostics' },
      ],
      detail: '/tmp/faktor-cli resolves outside the trusted install /opt/faktor/install',
    };
    snapshot.lastError = '[outside_trusted_install] /tmp/faktor-cli resolves outside the trusted install /opt/faktor/install';
  }
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
    snapshot.proofTaskState = null;
    snapshot.board = {
      available: false,
      source: 'none',
      revision: null,
      unread: null,
      reason: 'no run-family board yet',
      posts: [],
    };
  }
  // The real host derives this projection from the same snapshot. The
  // adversarial `advanced` tail models the legacy leak: if the chat renderer
  // ever copies it into the attention card, the DOM assertion catches it.
  snapshot.ux = {
    ...projectUx({
      daemon: snapshot.daemon,
      machineState: snapshot.machineState,
      activeRunId: snapshot.activeRunId,
      runs: snapshot.runs,
      proofTaskState: snapshot.proofTaskState,
      task: snapshot.task,
      verification: snapshot.verification,
      cockpit: null,
      changes: snapshot.changes,
      permissions: snapshot.permissions,
      streamStatus: snapshot.streamStatus,
      lastError: snapshot.lastError,
    }),
    advanced: [
      'machine state: ready_for_next_turn',
      'run state: Verifying',
      'proof state: verifying',
      'stream: open',
      'daemon: running',
    ],
  };
  return snapshot;
}

/** Strip the CSP + nonces, point the template at real files, inject stubs. */
export function buildHarnessHtml(
  empty = false,
  attachments = false,
  permissions = false,
  theme = 'dark',
  options = {},
) {
  const themeCss = THEMES[theme];
  if (!themeCss) {
    throw new Error(`unknown theme "${theme}" (expected dark or light)`);
  }
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
    ['${settingsStyleUri}', join(MEDIA, 'edition8.css')],
    ['${settingsScriptUri}', join(MEDIA, 'edition8.js')],
    ['${boardUri}', join(MEDIA, 'board-state.js')],
    ['${historyUri}', join(MEDIA, 'history-state.js')],
    ['${composerUri}', join(MEDIA, 'composer-state.js')],
    ['${scriptUri}', join(MEDIA, 'chat.js')],
  ]) {
    if (!html.includes(placeholder)) {
      throw new Error(`src/webview.ts lost the ${placeholder} placeholder`);
    }
    html = html.replace(placeholder, fileUrl(path));
  }
  if (!html.includes('${settingsMarkup}')) {
    throw new Error('src/webview.ts lost the ${settingsMarkup} placeholder');
  }
  html = html.replace(
    '${settingsMarkup}',
    renderEdition8AppMarkup({
      state: { snapshot: null, loading: false, error: null, focusControl: null },
      initialScreen: 'S29',
      host: 'vscode',
      mode: 'settings',
      omitEmptyBanner: true,
    }),
  );
  const themeStyle = `<style id="faktor-harness-theme">${themeCss}</style>`;
  const apiStub = `<script>window.__posted = []; window.acquireVsCodeApi = function () { return { postMessage: function (message) { window.__posted.push(message); } }; };</script>`;
  const snapshotJson = JSON.stringify(
    harnessSnapshot(empty, permissions, options.problem === true),
  ).replace(/</g, '\\u003c');
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
  const snapshotScript = options.skipSnapshot
    ? `<script>setTimeout(function () { window.__ready = true; }, 0);</script>`
    : `<script>
    window.postMessage({ type: 'snapshot', snapshot: ${snapshotJson} }, '*');${attachmentsScript}
    setTimeout(function () { window.__ready = true; }, 120);
  </script>`;
  html = html.replace('</head>', `${themeStyle}</head>`);
  if (typeof options.headScript === 'string' && options.headScript.length > 0) {
    html = html.replace('</head>', `<script>${options.headScript}</script></head>`);
  }
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

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export async function connect(wsUrl) {
  const ws = new WebSocket(wsUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', () => reject(new Error('websocket connect failed')), {
      once: true,
    });
  });
  return new Cdp(ws);
}

export async function launchChrome(chrome) {
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
      '--disable-dev-shm-usage',
      // Root containers (the pinned Chromium CI image) cannot use the
      // setuid sandbox; every other host keeps it.
      ...(process.getuid?.() === 0 || process.env.FAKTOR_CHROME_NO_SANDBOX === '1' ? ['--no-sandbox'] : []),
      'about:blank',
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  const wsUrl = await new Promise((resolve, reject) => {
    let stderr = '';
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      reject(new Error(`chrome did not report DevTools: ${stderr}`));
    }, 60000);
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
  try {
    const port = new URL(wsUrl).port;
    const list = await fetch(`http://127.0.0.1:${port}/json/list`).then((response) => response.json());
    const page = list.find((target) => target.type === 'page');
    if (!page) {
      throw new Error('no page target in chrome');
    }
    // Chrome reports the target URL on `localhost`, which may resolve to ::1
    // first in a Linux container while Chrome binds the DevTools listener on
    // 127.0.0.1 only. Pin the websocket host to the address we just proved
    // reachable with /json/list.
    const pageWsUrl = new URL(page.webSocketDebuggerUrl);
    pageWsUrl.hostname = '127.0.0.1';
    return { child, profile, pageWs: pageWsUrl.toString() };
  } catch (error) {
    child.kill('SIGKILL');
    throw error;
  }
}

// Surface/overlap probe: ONE header, NO tabs inside Chat, ONE problem card,
// and the composer in normal flow below a transcript that owns the remaining
// height. Every visible direct child of #work-panel is measured in DOM order
// so stacked cards can never be reported as overlapping.
const SURFACE_SCRIPT = `(() => {
  const rect = (el) => {
    const r = el.getBoundingClientRect();
    return { x: r.x, y: r.y, w: r.width, h: r.height, bottom: r.bottom, right: r.right };
  };
  const visible = (el) => !!el && !el.hidden && el.getClientRects().length > 0;
  const work = document.getElementById('work-panel');
  const workStyle = work ? getComputedStyle(work) : null;
  const surfaces = [];
  if (work) {
    for (const child of work.children) {
      if (!visible(child)) continue;
      surfaces.push({
        id: child.id || child.className,
        position: getComputedStyle(child).position,
        ...rect(child),
      });
    }
  }
  const columnXs = new Set(surfaces.map((surface) => Math.round(surface.x * 10) / 10));
  const layered = [];
  if (work) {
    for (const el of work.querySelectorAll('*')) {
      if (!visible(el) || el.closest('.sr-only')) continue;
      const position = getComputedStyle(el).position;
      if (position === 'absolute' || position === 'fixed' || position === 'sticky') {
        layered.push((el.id ? '#' + el.id : el.tagName.toLowerCase()) + ':' + position);
      }
    }
  }
  const attention = document.getElementById('attention-card');
  const composer = document.getElementById('composer');
  const entries = document.getElementById('entries');
  const technical = document.getElementById('attention-technical');
  const actionButtons = document.querySelectorAll('#attention-card-actions button');
  const header = document.querySelector('#app > header');
  return {
    headerCount: document.querySelectorAll('#app > header').length,
    header: visible(header) ? rect(header) : null,
    chatTabs: work
      ? work.querySelectorAll('[role="tab"], .nav-tab, #nav-switcher, [id^="nav-"]').length
      : 0,
    strayTabs: document.querySelectorAll('#nav-switcher, .nav-tab, #overflow-menu [role="tab"]').length,
    problemSurfaces: Array.from(document.querySelectorAll('[data-problem-surface]')).filter(visible).length,
    attention: visible(attention)
      ? {
          ...rect(attention),
          title: (document.getElementById('attention-card-title') || {}).textContent || '',
          cause: (document.getElementById('attention-card-cause') || {}).textContent || '',
          actions: actionButtons.length,
          technical: visible(technical),
        }
      : null,
    noticesVisible: document.querySelectorAll('#notices .notice').length,
    surfaces,
    workDisplay: workStyle ? workStyle.display : null,
    workDirection: workStyle ? workStyle.flexDirection : null,
    workColumns: columnXs.size,
    layered,
    workText: work ? String(work.innerText || work.textContent || '') : '',
    composer: composer ? { ...rect(composer), position: getComputedStyle(composer).position } : null,
    transcript: document.getElementById('transcript-card')
      ? rect(document.getElementById('transcript-card'))
      : null,
    entries: entries
      ? { ...rect(entries), overflowY: getComputedStyle(entries).overflowY }
      : null,
    innerHeight: window.innerHeight,
  };
})()`;

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
    const primary = el.tagName === 'BUTTON' && !secondary;
    out.push({
      name: name + (el.id ? '' : (el.className ? '.' + String(el.className).split(' ')[0] : '')),
      tag: el.tagName,
      w: Math.round(rect.width * 100) / 100,
      h: Math.round(rect.height * 100) / 100,
      primary,
    });
    boxes.push({ name, x: rect.x, y: rect.y, w: rect.width, h: rect.height, primary });
  }
  return {
    controls: out,
    boxes,
    innerWidth: window.innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
  };
})()`;

// VAL-ZOOM spill scan: any element whose box leaves the CSS-pixel budget while
// its own scroll containers cannot clip it is clipped content (a title, a
// label, an action). The view tablist (#nav-switcher, role="tablist") reflows
// to wrapped min-content rows below the compact range, so it is asserted here
// like every other surface. Elements inside an explicit horizontal scroller
// are contained and allowed.
const ZOOM_SPILL_SCRIPT = `(() => {
  const iw = window.innerWidth;
  const spills = [];
  for (const el of document.querySelectorAll('body *')) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    if (r.right <= iw + 0.5 && r.x >= -0.5) continue;
    let p = el.parentElement;
    let contained = false;
    while (p) {
      if (getComputedStyle(p).overflowX !== 'visible') {
        contained = true;
        break;
      }
      p = p.parentElement;
    }
    if (contained) continue;
    const label = el.tagName
      + (el.id ? '#' + el.id : el.className ? '.' + String(el.className).split(' ')[0] : '');
    spills.push(label + ' (x ' + Math.round(r.x * 10) / 10 + ', right ' + Math.round(r.right * 10) / 10 + ' vs innerWidth ' + iw + ')');
  }
  return spills.slice(0, 10);
})()`;

async function loadPage(cdp, url, width, height = 1000, deviceScaleFactor = 1) {
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  await cdp.send('Emulation.setDeviceMetricsOverride', {
    width,
    height,
    deviceScaleFactor,
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

/** Open (and optionally fill) the composer's Acceptance & contract rows. */
async function openContractComposer(cdp, fill) {
  await cdp.evaluate(`(() => {
    var details = document.getElementById('contract-options');
    if (details) { details.open = true; }
    ${
      fill
        ? `document.getElementById('contract-constraints').value = 'existing sessions remain valid\\nexisting API clients cannot break';
    document.getElementById('contract-invariants').value = 'revoked sessions cannot authenticate';
    document.getElementById('contract-risks').value = 'race conditions';
    document.getElementById('contract-obligations').value = 'replayed refresh tokens fail\\nlegacy sessions survive the migration';`
        : ''
    }
    return true;
  })()`);
}

/** Write one harness page and return its file: URL. */
function writeHarnessPage(html, prefix) {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  const path = join(dir, 'index.html');
  writeFileSync(path, html);
  return pathToFileURL(path).href;
}

/**
 * Fail-closed layout assertions for one measured page. `targets` enables the
 * >= 24x24 / 28x28 control-size checks; `documentOverflow` checks the page
 * scrollWidth; `primaryVisible` requires every primary decision control
 * (the view tablist included) to sit fully inside the viewport (the
 * VAL-ZOOM "primary decisions remain visible" observable).
 */
function assertLayout(result, context, failures, { targets = true, primaryVisible = false, documentOverflow = true } = {}) {
  if (documentOverflow && result.scrollWidth > result.innerWidth + 1) {
    failures.push(
      `${context}: horizontal overflow (scrollWidth ${result.scrollWidth} > innerWidth ${result.innerWidth})`,
    );
  }
  if (targets) {
    for (const control of result.controls) {
      const min = control.primary ? MAIN_MIN : OTHER_MIN;
      if (control.w < min - 0.51 || control.h < min - 0.51) {
        failures.push(
          `${context}: ${control.name} is ${control.w}x${control.h} (needs >= ${min}x${min})`,
        );
      }
    }
  }
  if (primaryVisible) {
    for (const box of result.boxes) {
      if (!box.primary) {
        continue;
      }
      if (box.w <= 0 || box.x < -0.5 || box.x + box.w > result.innerWidth + 0.5) {
        failures.push(
          `${context}: primary decision ${box.name} is not visible (x ${Math.round(box.x * 100) / 100}, w ${Math.round(box.w * 100) / 100} vs innerWidth ${result.innerWidth})`,
        );
      }
    }
  }
}

/**
 * The redesigned chat-surface contract. Every leg proves:
 *  - exactly ONE product header and ZERO tabs inside the chat body;
 *  - at most ONE visible problem surface, and when a problem is served it has
 *    a title, a short cause, exactly one action and a technical disclosure;
 *  - visible direct children of #work-panel do not overlap and stay in flow
 *    order (attention/status -> transcript -> composer);
 *  - the transcript owns scroll and the composer is static/relative with its
 *    bottom inside the viewport.
 * `problem` is true for the fixture that serves authority + blocked stream +
 * a raw error; false for the healthy populated surface.
 */
const FORBIDDEN_CHAT_STATE = [
  'ready_for_next_turn',
  'machine state:',
  'run state:',
  'proof state:',
  'stream:',
  'daemon:',
  'verification-record',
  'contract-state',
];

function assertSurface(result, context, failures, problem) {
  if (result.headerCount !== 1) {
    failures.push(`${context}: expected ONE header, found ${result.headerCount}`);
  }
  // The chat body is conversation-only: raw machine/daemon/run/proof/stream
  // vocabulary never appears, even when the snapshot serves it.
  const chatText = String(result.workText || '').toLowerCase();
  for (const forbidden of FORBIDDEN_CHAT_STATE) {
    if (chatText.includes(forbidden)) {
      failures.push(`${context}: chat body leaked machine state ${JSON.stringify(forbidden)}`);
    }
  }
  // ONE column, always: every chat region shares the same inline origin and
  // the work panel itself is a column flexbox (never a grid/side-by-side).
  if (result.workDisplay !== 'flex' || result.workDirection !== 'column') {
    failures.push(
      `${context}: chat must be one column (display ${String(result.workDisplay)}, direction ${String(result.workDirection)})`,
    );
  }
  if (result.workColumns > 1) {
    failures.push(`${context}: chat split into ${result.workColumns} columns`);
  }
  for (const surface of result.surfaces || []) {
    if (surface.position === 'absolute' || surface.position === 'fixed' || surface.position === 'sticky') {
      failures.push(`${context}: ${surface.id} is an overlapping ${surface.position} layer`);
    }
  }
  if ((result.layered || []).length > 0) {
    failures.push(`${context}: chat contains overlapping layers ${result.layered.join(', ')}`);
  }
  if (result.chatTabs !== 0 || result.strayTabs !== 0) {
    failures.push(
      `${context}: chat body must contain no view tabs (chat ${result.chatTabs}, global ${result.strayTabs})`,
    );
  }
  if (problem) {
    if (result.problemSurfaces !== 1) {
      failures.push(`${context}: expected ONE attention card, found ${result.problemSurfaces}`);
    }
    if (!result.attention) {
      failures.push(`${context}: the served problem must render the attention card`);
    } else {
      if (result.attention.title.trim().length === 0) {
        failures.push(`${context}: attention card has no title`);
      }
      if (result.attention.cause.trim().length === 0) {
        failures.push(`${context}: attention card has no short cause`);
      }
      if (result.attention.actions !== 1) {
        failures.push(`${context}: attention card must expose exactly one action (found ${result.attention.actions})`);
      }
      if (!result.attention.technical) {
        failures.push(`${context}: attention card must expose Technical details when a tail exists`);
      }
    }
    if (result.noticesVisible !== 0) {
      failures.push(`${context}: a visible notice card duplicates the attention surface (${result.noticesVisible})`);
    }
  } else if (result.problemSurfaces !== 0) {
    failures.push(`${context}: a healthy surface must omit the attention card (found ${result.problemSurfaces})`);
  }
  if (result.header && result.surfaces.length > 0 && result.surfaces[0].y + 0.5 < result.header.bottom) {
    failures.push(`${context}: first surface overlaps the header`);
  }
  for (let i = 1; i < result.surfaces.length; i += 1) {
    const previous = result.surfaces[i - 1];
    const current = result.surfaces[i];
    if (previous.bottom > current.y + 0.5) {
      failures.push(
        `${context}: ${previous.id} overlaps ${current.id} (bottom ${Math.round(previous.bottom * 10) / 10} > top ${Math.round(current.y * 10) / 10})`,
      );
    }
  }
  if (!result.composer) {
    failures.push(`${context}: composer is missing`);
  } else {
    if (result.composer.position === 'fixed' || result.composer.position === 'absolute' || result.composer.position === 'sticky') {
      failures.push(`${context}: composer must stay in normal flow, found position ${result.composer.position}`);
    }
    if (result.composer.bottom > result.innerHeight + 0.5) {
      failures.push(
        `${context}: composer leaves the viewport (bottom ${Math.round(result.composer.bottom * 10) / 10} > ${result.innerHeight})`,
      );
    }
  }
  if (!result.transcript || result.transcript.h < 40) {
    failures.push(`${context}: transcript does not fill the remaining height (h ${result.transcript ? result.transcript.h : 'missing'})`);
  }
  if (!result.entries || result.entries.overflowY !== 'auto') {
    failures.push(`${context}: transcript must own scrolling (entries overflow-y ${result.entries ? result.entries.overflowY : 'missing'})`);
  }
}

/** Fresh load, Tab, then prove the keyboard focus ring renders. */
async function checkKeyboardFocus(cdp, context, failures) {
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
      `${context}: keyboard focus ring missing on ${focus.tag}#${focus.id}: outline ${focus.outlineStyle} ${focus.outlineWidth}`,
    );
  }
  return focus;
}

/** Fresh load, pointer click, then prove pointer focus does NOT ring. */
async function checkPointerSuppression(cdp, context, failures) {
  const triggerBox = await cdp.evaluate(`(() => {
    const r = document.getElementById('btn-overflow').getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  })()`);
  await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: triggerBox.x, y: triggerBox.y, button: 'left', clickCount: 1 });
  await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: triggerBox.x, y: triggerBox.y, button: 'left', clickCount: 1 });
  const pointer = await cdp.evaluate(`(() => {
    const el = document.activeElement;
    const cs = getComputedStyle(el);
    return { id: el.id, outlineStyle: cs.outlineStyle };
  })()`);
  if (pointer.outlineStyle !== 'none') {
    failures.push(`${context}: pointer click on #btn-overflow left outline ${pointer.outlineStyle}`);
  }
  return pointer;
}

function parseArgs(argv) {
  const args = {
    check: false,
    screenshots: false,
    empty: false,
    attachments: false,
    permissions: false,
    diagnostics: false,
    finish: false,
    contract: false,
    error: false,
    problem: false,
    theme: 'both',
    prefix: 'shot',
    out: SHOT_DIR,
  };
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--check') args.check = true;
    else if (argv[i] === '--screenshots') args.screenshots = true;
    else if (argv[i] === '--empty') args.empty = true;
    else if (argv[i] === '--attachments') args.attachments = true;
    else if (argv[i] === '--permissions') args.permissions = true;
    else if (argv[i] === '--diagnostics') args.diagnostics = true;
    else if (argv[i] === '--finish') args.finish = true;
    else if (argv[i] === '--contract') args.contract = true;
    else if (argv[i] === '--error') args.error = true;
    else if (argv[i] === '--problem') args.problem = true;
    else if (argv[i] === '--theme') args.theme = argv[++i];
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
  const requestedTheme = args.theme || 'both';
  const themeNames = requestedTheme === 'both' ? ['dark', 'light'] : [requestedTheme];
  for (const theme of themeNames) {
    if (!THEMES[theme]) {
      throw new Error(`unknown theme "${theme}" (expected dark, light or both)`);
    }
  }
  // One page per theme. Screenshots keep the historical dark render unless
  // --theme light was selected explicitly.
  const pageUrls = new Map();
  const problemUrls = new Map();
  for (const theme of themeNames) {
    pageUrls.set(
      theme,
      writeHarnessPage(
        buildHarnessHtml(
          args.empty === true,
          args.attachments === true,
          args.permissions === true,
          theme,
          { problem: false },
        ),
        'faktor-render-page-',
      ),
    );
    problemUrls.set(
      theme,
      writeHarnessPage(
        buildHarnessHtml(
          args.empty === true,
          args.attachments === true,
          args.permissions === true,
          theme,
          { problem: true },
        ),
        'faktor-render-page-problem-',
      ),
    );
  }
  const pageUrl =
    (args.problem ? problemUrls : pageUrls).get('dark') || (args.problem ? problemUrls : pageUrls).get(themeNames[0]);

  const { child, pageWs } = await launchChrome(chrome);
  const cdp = await connect(pageWs);
  const failures = [];
  const measured = new Map();
  let layouts = 0;
  try {
    if (args.check) {
      // Theme leg: the FULL populated-surface assertion set runs at every
      // width under every selected theme.
      for (const theme of themeNames) {
        const themeUrl = pageUrls.get(theme);
        for (const width of WIDTHS) {
          await loadPage(cdp, themeUrl, width);
          if (args.contract === true) {
            await openContractComposer(cdp, false);
          }
          const result = await cdp.evaluate(MEASURE_SCRIPT);
          layouts += 1;
          assertLayout(result, `${theme}@${width}px`, failures);
          const surface = await cdp.evaluate(SURFACE_SCRIPT);
          assertSurface(surface, `${theme}@${width}px`, failures, false);
          for (const control of result.controls) {
            measured.set(`${theme}/${control.name}`, control);
          }
        }
      }
      // The PROBLEM leg: service authority + a blocked stream + a raw error
      // must merge into ONE attention card with no overlap at every width.
      for (const theme of themeNames) {
        const themeUrl = problemUrls.get(theme);
        for (const width of WIDTHS) {
          await loadPage(cdp, themeUrl, width);
          const result = await cdp.evaluate(MEASURE_SCRIPT);
          layouts += 1;
          assertLayout(result, `${theme} problem@${width}px`, failures);
          const surface = await cdp.evaluate(SURFACE_SCRIPT);
          assertSurface(surface, `${theme} problem@${width}px`, failures, true);
        }
      }
      // The EMPTY state (welcome + preflight card visible) is its own layout:
      // exercise the same widths under every theme so the narrow single-column
      // rules and the preflight actions are measured, not hidden behind a
      // populated transcript.
      for (const theme of themeNames) {
        const emptyUrl = writeHarnessPage(
          buildHarnessHtml(true, args.attachments === true, args.permissions === true, theme),
          'faktor-render-page-empty-',
        );
        for (const width of WIDTHS) {
          await loadPage(cdp, emptyUrl, width);
          const result = await cdp.evaluate(MEASURE_SCRIPT);
          layouts += 1;
          assertLayout(result, `${theme} empty@${width}px`, failures);
          const surface = await cdp.evaluate(SURFACE_SCRIPT);
          assertSurface(surface, `${theme} empty@${width}px`, failures, false);
        }
      }
      // Keyboard focus ring + pointer suppression: fresh load per theme,
      // press Tab / click, then inspect the focused node.
      const focusStates = [];
      for (const theme of themeNames) {
        const themeUrl = pageUrls.get(theme);
        await loadPage(cdp, themeUrl, 480);
        const focus = await checkKeyboardFocus(cdp, `${theme}@480px focus`, failures);
        await loadPage(cdp, themeUrl, 480);
        const pointer = await checkPointerSuppression(cdp, `${theme}@480px pointer`, failures);
        focusStates.push({ theme, focus, pointer });
      }
      // DPR leg: the same overflow/target/focus invariants at device pixel
      // ratios 1.5 and 2.0 (HiDPI rendering scales; the CSS layout stays
      // width-driven).
      for (const theme of themeNames) {
        const themeUrl = pageUrls.get(theme);
        for (const dpr of DPRS) {
          const dprLabel = dpr.toFixed(1);
          for (const width of WIDTHS) {
            await loadPage(cdp, themeUrl, width, 1000, dpr);
            const result = await cdp.evaluate(MEASURE_SCRIPT);
            layouts += 1;
            assertLayout(result, `${theme} dpr${dprLabel}@${width}px`, failures);
            const surface = await cdp.evaluate(SURFACE_SCRIPT);
            assertSurface(surface, `${theme} dpr${dprLabel}@${width}px`, failures, false);
          }
          await loadPage(cdp, themeUrl, 480, 1000, dpr);
          await checkKeyboardFocus(cdp, `${theme} dpr${dprLabel}@480px focus`, failures);
          await loadPage(cdp, themeUrl, 480, 1000, dpr);
          await checkPointerSuppression(cdp, `${theme} dpr${dprLabel}@480px pointer`, failures);
        }
      }
      // VAL-ZOOM leg: IDE UI scale 150%/200% shrinks the CSS-pixel budget the
      // webview sees (viewport width / zoom, floored at 120px), so the layout
      // is measured at that reduced width. The document scrollWidth is not the
      // right probe at the floor (wrapped rows are intended), so this leg
      // asserts the surfaces directly: no uncontained element — the reflowing
      // view switcher included — spills past the budget, and every primary
      // decision stays visible.
      for (const theme of themeNames) {
        const themeUrl = pageUrls.get(theme);
        for (const zoom of ZOOMS) {
          for (const width of WIDTHS) {
            const effective = Math.max(ZOOM_MIN_WIDTH, Math.floor(width / zoom));
            await loadPage(cdp, themeUrl, effective);
            const result = await cdp.evaluate(MEASURE_SCRIPT);
            const context = `${theme} zoom${zoom * 100}@${effective}px`;
            layouts += 1;
            assertLayout(result, context, failures, {
              targets: false,
              primaryVisible: true,
              documentOverflow: false,
            });
            const surface = await cdp.evaluate(SURFACE_SCRIPT);
            assertSurface(surface, context, failures, false);
            const spills = await cdp.evaluate(ZOOM_SPILL_SCRIPT);
            for (const spill of spills) {
              failures.push(`${context}: ${spill} overflows the main action surfaces`);
            }
          }
        }
      }
      console.log(`RENDER CHECK themes: ${themeNames.join(', ')}`);
      console.log(`RENDER CHECK layouts asserted: ${layouts}`);
      console.log('RENDER CHECK measured controls (largest width):');
      for (const control of measured.values()) {
        console.log(`  ${control.primary ? 'primary' : 'other  '} ${control.name}: ${control.w}x${control.h}`);
      }
      for (const { theme, focus, pointer } of focusStates) {
        console.log(`RENDER CHECK focus (${theme}): ${focus.tag}#${focus.id} outline ${focus.outlineWidth} ${focus.outlineStyle} ${focus.outlineColor} offset ${focus.outlineOffset}`);
        console.log(`RENDER CHECK pointer suppression (${theme}): outline ${pointer.outlineStyle}`);
      }
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
        if (args.diagnostics === true) {
          await cdp.evaluate(
            `(() => { const b = document.getElementById('btn-overflow'); b.click(); if (typeof b.blur === 'function') b.blur(); return true; })()`,
          );
        }
        if (args.finish === true) {
          await cdp.evaluate(`document.getElementById('finish-options').open = true`);
        }
        if (args.contract === true) {
          await openContractComposer(cdp, true);
        }
        if (args.error === true) {
          await cdp.evaluate(`(() => {
            window.dispatchEvent(new MessageEvent('message', { data: { type: 'userError', error: {
              kind: 'stream_gap',
              summary: 'Live updates paused',
              hint: 'Reconnect to resume from the last event. Nothing is skipped.',
              action: { key: 'reconnectStream', label: 'Reconnect' },
              technical: '[protocol_blocked] durable frame 5 declares unsupported event version 2',
              code: 'protocol_blocked',
            } } }));
            return true;
          })()`);
        }
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
  // The local amd64 CI agent runs this image under QEMU, where Chrome's
  // DevTools socket can be starved or close once. Retry a failed launch/read
  // a bounded number of times; assertion failures are returned, not retried.
  let lastError = null;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    try {
      const result = await runHarness(args);
      process.exitCode = result.status;
      return;
    } catch (error) {
      lastError = error;
      if (attempt < 3) {
        console.error(`render-webview: attempt ${attempt} failed: ${error.message}; retrying`);
        await sleep(2000);
      }
    }
  }
  throw lastError;
}

const isMain = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isMain) {
  main().catch((error) => {
    console.error(`render-webview error: ${error.message}`);
    process.exitCode = 1;
  });
}
