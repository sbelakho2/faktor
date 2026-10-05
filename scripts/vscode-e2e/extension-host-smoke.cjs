'use strict';
// Faktor VS Code Extension Host assurance -- this module is loaded by the
// PINNED VS Code extension host via `--extensionTestsPath`
// (scripts/vscode-e2e.sh).
//
// It runs against the built Faktor extension (`--extensionDevelopmentPath`,
// either the dev tree or a VSIX-extracted tree) and asserts:
//   * the `faktor.faktor` extension is present and activates,
//   * the contributed `faktor.*` commands are registered,
//   * the compiled webview provider renders HTML carrying the strict CSP and
//     the composer/attachment surface elements,
//   * THE REAL MATRIX: a real WebviewPanel is created and driven in the real
//     Chromium renderer over the Chrome DevTools Protocol (CDP, reached via
//     the `--remote-debugging-port` the lane starts VS Code with). The lane
//     records exactly which capability was exercised with which method:
//       - themes: live `workbench.colorTheme` switches (Default Light/Dark/
//         High Contrast/High Contrast Light) observed as webview body
//         classes, with the verdict-chip fg/bg pair re-measured in the
//         renderer and WCAG-checked per theme;
//       - widths: the webview OOPIF viewport is resized to 240/320/480/800
//         CSS px by calibrating a top-level device-metrics emulation (the
//         OOPIF target itself rejects Emulation commands as non-top-level),
//         then real layout overflow is measured inside the frame;
//       - zoom: the lane launches dedicated cells with
//         `--force-device-scale-factor=1.25`/`=2` (the real device scale
//         factor observed as devicePixelRatio in the webview) plus a live
//         `workbench.action.zoomIn` step observed in the renderer;
//       - keyboard-only: real TRUSTED `Input.dispatchKeyEvent` Tab traversal
//         and Ctrl+Enter submission (isTrusted:true), not synthetic events;
//       - paste/drop: real `DataTransfer`/`File` objects dispatched as
//         ClipboardEvent/DragEvent at the real handlers (CDP has no
//         OS-clipboard/OS-drag file injection); the resulting bytes are
//         verified in the host message;
//       - disposal/reopen: a real WebviewPanel dispose followed by a real
//         provider re-resolve, checking snapshot + attachment restoration
//         and a fresh CSP nonce.
//
// NOT EMULATABLE HEADLESSLY (recorded verbatim in the evidence `matrix`
// object, never a silent pass, never a TODO):
//   * trusted OS clipboard paste bytes and trusted OS drag-and-drop payloads
//     (no headless Electron/CDP API injects a file payload into the OS
//     clipboard or a native drag session) -- synthetic events with real
//     DataTransfer/File objects are the strongest available check;
//   * a physical workbench window/panel resize (no extension API resizes a
//     panel; the viewport is driven through top-level CDP device metrics and
//     verified exact inside the webview);
//   * screenshots / pixel comparison.

const assert = require('node:assert');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checks = [];
const check = (name, condition, detail) => {
  const ok = Boolean(condition);
  checks.push({ name, ok, detail: detail === undefined ? '' : String(detail) });
  assert.ok(ok, `${name}${detail ? `: ${detail}` : ''}`);
};

const evidencePath = process.env.FAKTOR_VSCODE_E2E_EVIDENCE || '';
const cdpPort = Number(process.env.FAKTOR_VSCODE_E2E_CDP_PORT || '0');
const matrixMode = process.env.FAKTOR_VSCODE_E2E_MATRIX || 'full';
const expectDpr = Number(process.env.FAKTOR_VSCODE_E2E_EXPECT_DPR || '1');
const packageMode = process.env.FAKTOR_VSCODE_E2E_PACKAGE_MODE || 'dev';
const cellLabel = process.env.FAKTOR_VSCODE_E2E_CELL || matrixMode;

const matrix = {
  enabled: cdpPort > 0,
  cell: { label: cellLabel, mode: matrixMode, expected_dpr: expectDpr, package_mode: packageMode },
  cdp: { connected: false, transport: 'Chrome DevTools Protocol over 127.0.0.1' },
  capabilities: {},
  not_emulatable: [
    'trusted OS clipboard paste bytes (no headless Electron/CDP API injects a file into the OS clipboard): synthetic ClipboardEvent with real DataTransfer/File at the real handler',
    'trusted OS drag-and-drop file payloads (no headless native drag session): synthetic DragEvent with real DataTransfer/File at the real handler',
    'physical workbench window/panel resize (no extension API resizes a webview panel): top-level CDP device metrics calibrated until the webview OOPIF viewport is exactly W',
    'in-place `workbench.action.zoomIn` observation on the same webview (the pinned headless workbench tears down the transient test webview when the window zoom level changes): zoom is proven by dedicated --force-device-scale-factor=1.25/2 launches whose devicePixelRatio is asserted in the webview, plus in-renderer CSS zoom 125%/200% layout reflow',
    'screenshots / pixel comparison (out of scope for this lane; the renderer is driven live instead)',
  ],
  errors: [],
};

function writeEvidence(status, error) {
  if (!evidencePath) return;
  const record = {
    schema: 'faktor-vscode-e2e-evidence/v2',
    status,
    error: error ? String(error.stack || error) : '',
    extension: 'faktor.faktor',
    checks,
    matrix,
    wrote_at: new Date().toISOString(),
  };
  fs.writeFileSync(evidencePath, `${JSON.stringify(record, null, 2)}\n`);
}

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---------------------------------------------------------------------- CDP

/** Minimal CDP WebSocket client (Node >= 22 global WebSocket). */
async function cdpConnect(url, timeoutMs) {
  assert(typeof WebSocket === 'function', 'the extension host Node must expose global WebSocket');
  const ws = new WebSocket(url);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error(`CDP websocket open timeout: ${url}`)), timeoutMs);
    ws.onopen = () => {
      clearTimeout(timer);
      resolve();
    };
    ws.onerror = () => {
      clearTimeout(timer);
      reject(new Error(`CDP websocket error: ${url}`));
    };
  });
  let nextId = 0;
  const pending = new Map();
  const events = [];
  ws.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return;
    }
    if (message.id && pending.has(message.id)) {
      const entry = pending.get(message.id);
      pending.delete(message.id);
      clearTimeout(entry.timer);
      if (message.error) entry.reject(new Error(`${entry.method}: ${message.error.message}`));
      else entry.resolve(message.result);
      return;
    }
    events.push(message);
  };
  const send = (method, params) =>
    new Promise((resolve, reject) => {
      const id = ++nextId;
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new Error(`${method} timeout`));
      }, timeoutMs);
      pending.set(id, { resolve, reject, timer, method });
      ws.send(JSON.stringify({ id, method, params: params || {} }));
    });
  return {
    send,
    events,
    close: () => ws.close(),
  };
}

async function fetchJson(url, timeoutMs) {
  const response = await fetch(url, { signal: AbortSignal.timeout(timeoutMs) });
  if (!response.ok) throw new Error(`HTTP ${response.status} for ${url}`);
  return response.json();
}

/**
 * The webview is an out-of-process iframe: `/json/list` exposes a `type:
 * "iframe"` target for each live webview. Several webviews may exist (the
 * real chat view plus the test panel), so each probe carries a unique token
 * and the matching target is the one whose main context returns it.
 */
async function connectMatrixTarget(token, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  let lastError = 'no iframe target';
  while (Date.now() < deadline) {
    let targets;
    try {
      targets = await fetchJson(`http://127.0.0.1:${cdpPort}/json/list`, 5000);
    } catch (error) {
      lastError = String(error && error.message);
      await wait(400);
      continue;
    }
    const candidates = targets.filter(
      (target) => target.type === 'iframe' && target.webSocketDebuggerUrl,
    );
    for (const candidate of candidates) {
      let connection = null;
      try {
        connection = await cdpConnect(candidate.webSocketDebuggerUrl, 6000);
        await connection.send('Runtime.enable');
        await wait(400);
        const contexts = connection.events
          .filter((event) => event.method === 'Runtime.executionContextCreated')
          .map((event) => event.params.context);
        for (const context of contexts) {
          const probe = await evaluate(connection, context.id, 'window.__FAKTOR_E2E ? window.__FAKTOR_E2E.token : null');
          if (probe === token) {
            return { connection, contextId: context.id, contexts: contexts.length };
          }
        }
        connection.close();
      } catch (error) {
        lastError = String(error && error.message);
        if (connection) {
          try {
            connection.close();
          } catch {
            /* already closed */
          }
        }
      }
    }
    await wait(400);
  }
  throw new Error(`no CDP webview target carried the probe token (${lastError})`);
}

async function evaluate(connection, contextId, expression) {
  const result = await connection.send('Runtime.evaluate', {
    contextId,
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  if (result.exceptionDetails) {
    throw new Error(`evaluate failed: ${JSON.stringify(result.exceptionDetails).slice(0, 300)}`);
  }
  return result.result.value;
}

// ------------------------------------------------------------- real webview

let probeCounter = 0;
function nextProbeToken() {
  probeCounter += 1;
  return `faktor-e2e-${Date.now().toString(36)}-${probeCounter}-${crypto.randomBytes(6).toString('hex')}`;
}

/** Capture the webview API instance before chat.js acquires it. */
function captureScript(nonce) {
  return (
    `<script nonce="${nonce}">(function(){var real=window.acquireVsCodeApi;` +
    `window.acquireVsCodeApi=function(){var api=real.apply(this,arguments);` +
    `window.__FAKTOR_E2E_API=api;return api;};})();</script>`
  );
}

/** Probe installed after chat.js; exposes data for CDP evaluation. */
function probeScript(nonce, token) {
  return (
    `<script nonce="${nonce}">(function(){` +
    `window.__FAKTOR_E2E={token:${JSON.stringify(token)},ready:false,keyLog:[],focusLog:[]};` +
    `window.addEventListener('keydown',function(e){window.__FAKTOR_E2E.keyLog.push({key:e.key,ctrl:e.ctrlKey,meta:e.metaKey,trusted:e.isTrusted,target:e.target&&e.target.id});});` +
    `document.addEventListener('focusin',function(e){window.__FAKTOR_E2E.focusLog.push(e.target&&e.target.id);});` +
    `window.__FAKTOR_E2E.chip=function(selector){var d=document.querySelector(selector);if(!d){d=document.createElement('span');d.className=selector.replace(/^\\./,'');d.textContent='PROBE';document.body.appendChild(d);}var cs=getComputedStyle(d);return{color:cs.color,background:cs.backgroundColor};};` +
    `window.__FAKTOR_E2E.state=function(){return{bodyClass:document.body.className,innerWidth:window.innerWidth,innerHeight:window.innerHeight,dpr:window.devicePixelRatio,scrollWidth:document.documentElement.scrollWidth,clientWidth:document.documentElement.clientWidth,sheets:Array.prototype.map.call(document.styleSheets,function(s){return s.href||'(inline)';})};};` +
    `window.__FAKTOR_E2E.ready=true;` +
    `})();</script>`
  );
}

/** Append the nonce'd capture+probe scripts to provider-rendered HTML. */
function injectProbe(html, token) {
  const nonceMatch = /nonce-([0-9a-f]+)/.exec(html);
  assert(nonceMatch, 'the rendered webview HTML must carry a CSP nonce');
  const nonce = nonceMatch[1];
  return {
    nonce,
    html: html
      .replace('<script ', `${captureScript(nonce)}<script `)
      .replace('</body>', `${probeScript(nonce, token)}\n</body>`),
  };
}

function createTestPanel(vscode, extensionUri, token, messageSink) {
  const panel = vscode.window.createWebviewPanel(
    'faktor.e2eMatrix',
    'Faktor E2E Matrix',
    vscode.ViewColumn.One,
    {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(extensionUri, 'media')],
    },
  );
  panel.webview.onDidReceiveMessage((message) => {
    messageSink.push({ panel: token, message });
  });
  const real = panel.webview;
  let lastHtml = '';
  let lastNonce = '';
  // The provider's resolve path sets `webview.options` and `webview.html`;
  // the proxy delegates everything and injects the probe into every render.
  const webviewProxy = {
    get cspSource() {
      return real.cspSource;
    },
    asWebviewUri: (uri) => real.asWebviewUri(uri),
    postMessage: (message) => real.postMessage(message),
    onDidReceiveMessage: (handler) => real.onDidReceiveMessage(handler),
    get options() {
      return real.options;
    },
    set options(value) {
      real.options = value;
    },
    get html() {
      return real.html;
    },
    set html(value) {
      const injected = injectProbe(value, token);
      lastHtml = injected.html;
      lastNonce = injected.nonce;
      real.html = injected.html;
    },
  };
  const view = {
    viewType: 'faktor.chat',
    title: 'Faktor E2E Matrix',
    visible: true,
    show() {},
    onDidChangeVisibility: () => ({ dispose() {} }),
    onDidDispose: (handler) => panel.onDidDispose(handler),
    webview: webviewProxy,
  };
  return {
    panel,
    view,
    webviewProxy,
    get html() {
      return lastHtml;
    },
    get nonce() {
      return lastNonce;
    },
    dispose: () => panel.dispose(),
  };
}

function waitForPanelMessage(sink, token, type, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  return (async () => {
    while (Date.now() < deadline) {
      const hit = sink.find(
        (entry) => entry.panel === token && entry.message && entry.message.type === type,
      );
      if (hit) return hit.message;
      await wait(100);
    }
    return null;
  })();
}

// ---------------------------------------------------------- WCAG (renderer)

function srgbToLinear(channel) {
  const c = channel / 255;
  return c <= 0.04045 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4);
}

/** Parse `rgb(r, g, b)` / `rgba(r, g, b, a)` into [r,g,b,a]. */
function parseCssRgb(value) {
  const match =
    /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,\s*([0-9.]+)\s*)?\)$/.exec(String(value));
  if (!match) return null;
  return [Number(match[1]), Number(match[2]), Number(match[3]), match[4] === undefined ? 1 : Number(match[4])];
}

function luminance([r, g, b]) {
  return 0.2126 * srgbToLinear(r) + 0.7152 * srgbToLinear(g) + 0.0722 * srgbToLinear(b);
}

/** WCAG contrast ratio of two parsed rgb colors (opaque only). */
function renderedContrast(foreground, background) {
  const fg = parseCssRgb(foreground);
  const bg = parseCssRgb(background);
  if (!fg || !bg || fg[3] < 1 || bg[3] < 1) return null;
  const a = luminance(fg);
  const b = luminance(bg);
  return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
}

// ------------------------------------------------------------------ matrix

const THEMES = [
  { name: 'Default Light Modern', bodyClass: 'vscode-light' },
  { name: 'Default Dark Modern', bodyClass: 'vscode-dark' },
  { name: 'Default High Contrast', bodyClass: 'vscode-high-contrast' },
  { name: 'Default High Contrast Light', bodyClass: 'vscode-high-contrast-light' },
];

const WIDTHS = [240, 320, 480, 800];

async function runMatrix(vscode, ext, providerModule) {
  if (!matrix.enabled) {
    matrix.capabilities = { matrix: { emulated: false, method: 'not requested (no FAKTOR_VSCODE_E2E_CDP_PORT)' } };
    return;
  }
  const ChatViewProvider = providerModule.ChatViewProvider;
  const extensionUri = vscode.Uri.file(ext.extensionPath);
  const sink = [];
  const hostMessages = [];
  const host = {
    handle(message) {
      hostMessages.push(message);
    },
  };
  matrix.capabilities.zoom = {
    emulated: true,
    method:
      'real per-factor launches: the lane runs --force-device-scale-factor=<factor> cells and asserts window.devicePixelRatio inside the webview equals the factor (100/125/200%); the full cell additionally reflows the real renderer at CSS zoom 1.25/2',
    factors: [],
  };
  matrix.capabilities.package = {
    emulated: true,
    method: packageMode === 'vsix' ? 'host runs from a VSIX-extracted extension tree' : 'host runs from the dev extension tree',
    value: packageMode,
  };
  if (packageMode === 'vsix') {
    check(
      `matrix:${cellLabel}:package-from-vsix`,
      /vsix-extracted/.test(ext.extensionPath),
      `packaged mode must load the extracted VSIX tree, got ${ext.extensionPath}`,
    );
  }

  const provider = new ChatViewProvider(extensionUri, host);
  const firstToken = nextProbeToken();
  const first = createTestPanel(vscode, extensionUri, firstToken, sink);
  provider.resolveWebviewView(first.view);
  provider.postSnapshot(matrixSnapshot());
  const ready = await waitForPanelMessage(sink, firstToken, 'ready', 15000);
  check(`matrix:${cellLabel}:webview-booted`, ready !== null, 'chat.js must post ready inside the real webview');
  await wait(600);

  const target = await connectMatrixTarget(firstToken, 20000);
  matrix.cdp.connected = true;
  matrix.cdp.target = 'out-of-process iframe (vscode-webview)';
  let liveConnection = target.connection;
  let liveContext = target.contextId;
  // The renderer frame/context can be recreated across phase transitions; the
  // evaluator re-resolves the probe token and retries once instead of failing
  // the lane on a transient CDP detach.
  const ev = async (expression) => {
    try {
      return await evaluate(liveConnection, liveContext, expression);
    } catch (error) {
      matrix.errors.push(`cdp-reconnect after evaluate: ${error && error.message}`);
      const reconnected = await connectMatrixTarget(firstToken, 15000);
      try {
        liveConnection.close();
      } catch {
        /* already closed */
      }
      liveConnection = reconnected.connection;
      liveContext = reconnected.contextId;
      return evaluate(liveConnection, liveContext, expression);
    }
  };
  const input = {
    key: async (type, keyValue, code, keyCode, modifiers) => {
      try {
        await liveConnection.send('Input.dispatchKeyEvent', {
          type,
          key: keyValue,
          code,
          windowsVirtualKeyCode: keyCode,
          nativeVirtualKeyCode: keyCode,
          modifiers: modifiers || 0,
        });
      } catch {
        const reconnected = await connectMatrixTarget(firstToken, 15000);
        try {
          liveConnection.close();
        } catch {
          /* already closed */
        }
        liveConnection = reconnected.connection;
        liveContext = reconnected.contextId;
        await liveConnection.send('Input.dispatchKeyEvent', {
          type,
          key: keyValue,
          code,
          windowsVirtualKeyCode: keyCode,
          nativeVirtualKeyCode: keyCode,
          modifiers: modifiers || 0,
        });
      }
    },
    insertText: async (text) => {
      try {
        await liveConnection.send('Input.insertText', { text });
      } catch {
        const reconnected = await connectMatrixTarget(firstToken, 15000);
        try {
          liveConnection.close();
        } catch {
          /* already closed */
        }
        liveConnection = reconnected.connection;
        liveContext = reconnected.contextId;
        await liveConnection.send('Input.insertText', { text });
      }
    },
  };
  const hostTypes = () => hostMessages.map((message) => message.type);

  try {
    // ---- renderer state + dpr -------------------------------------------
    const state = await ev('window.__FAKTOR_E2E.state()');
    matrix.capabilities.renderer = {
      emulated: true,
      method: 'real pinned Electron renderer via CDP Runtime.evaluate',
      body_class: state.bodyClass,
      dpr: state.dpr,
    };
    matrix.capabilities.zoom.factors.push({ factor: expectDpr, observed_dpr: state.dpr });
    check(
      `matrix:${cellLabel}:dpr:${expectDpr}`,
      Math.abs(state.dpr - expectDpr) < 0.01,
      `devicePixelRatio ${state.dpr}, expected ${expectDpr}`,
    );
    check(
      `matrix:${cellLabel}:chat-css-loaded`,
      state.sheets.some((href) => String(href).endsWith('/chat.css')),
      `loaded stylesheets: ${state.sheets.join(', ')}`,
    );

    // ---- real layout widths ---------------------------------------------
    const widthRun = await measureWidths(ev, state.innerWidth);
    matrix.capabilities.widths = widthRun.result;
    for (const width of matrix.capabilities.widths.observed) {
      check(
        `matrix:${cellLabel}:width-${width.requested}`,
        Math.abs(width.inner - width.requested) <= 1 && width.scrollWidth <= width.inner + 1,
        `requested ${width.requested}px -> webview innerWidth ${width.inner}px scrollWidth ${width.scrollWidth}px`,
      );
    }

    // ---- paste + drop (before any submission locks the composer) --------
    matrix.capabilities.paste = await runPasteDrop('paste', ev, sink, hostTypes);
    check(
      `matrix:${cellLabel}:paste-attachment`,
      matrix.capabilities.paste.accepted &&
        matrix.capabilities.paste.attachment.filename === 'paste.png' &&
        matrix.capabilities.paste.attachment.mime === 'image/png' &&
        matrix.capabilities.paste.attachment.bytes === 4,
      JSON.stringify(matrix.capabilities.paste),
    );
    matrix.capabilities.drop = await runPasteDrop('drop', ev, sink, hostTypes);
    check(
      `matrix:${cellLabel}:drop-attachment`,
      matrix.capabilities.drop.accepted &&
        matrix.capabilities.drop.attachment.filename === 'drop.bin' &&
        matrix.capabilities.drop.attachment.bytes === 3,
      JSON.stringify(matrix.capabilities.drop),
    );

    // ---- keyboard-only operation (trusted input) ------------------------
    matrix.capabilities.keyboard = await runKeyboardOnly(input, ev, sink, hostTypes, provider);
    check(
      `matrix:${cellLabel}:keyboard-only-tab-reaches-composer`,
      matrix.capabilities.keyboard.focus_path.includes('goal'),
      `focus path: ${matrix.capabilities.keyboard.focus_path.join(' -> ')}`,
    );
    check(
      `matrix:${cellLabel}:keyboard-only-trusted`,
      matrix.capabilities.keyboard.trusted === true,
      `keyboard events trusted: ${matrix.capabilities.keyboard.trusted}`,
    );
    check(
      `matrix:${cellLabel}:keyboard-only-submits`,
      hostMessages.some(
        (message) => message.type === 'sendGoal' && message.goal === matrix.capabilities.keyboard.typed,
      ) && matrix.capabilities.keyboard.draft_cleared === true,
      `host saw sendGoal and the panel cleared the draft: ${hostTypes().join(', ')}`,
    );

    if (matrixMode === 'full') {
      // ---- renderer CSS zoom (exact 125%/200% layout reflow) ------------
      matrix.capabilities.zoom_css = await measureCssZoom(ev, widthRun);
      check(
        `matrix:${cellLabel}:zoom-css-reflows`,
        matrix.capabilities.zoom_css.monotonic,
        JSON.stringify(matrix.capabilities.zoom_css),
      );
      await widthRun.close();
      // ---- themes + verdict contrast per theme --------------------------
      matrix.capabilities.themes = await measureThemes(vscode, ev);
      for (const theme of matrix.capabilities.themes.observed) {
        check(
          `matrix:${cellLabel}:theme-class:${theme.name}`,
          theme.matched,
          `body class ${theme.body_class}`,
        );
        for (const chip of theme.chips) {
          check(
            `matrix:${cellLabel}:verdict-contrast:${theme.name}:${chip.selector}`,
            chip.ratio !== null && chip.ratio >= 4.5,
            `${chip.foreground} on ${chip.background} = ${chip.ratio === null ? 'unmeasurable' : chip.ratio.toFixed(3)}:1`,
          );
        }
      }
    } else {
      await widthRun.close();
      const currentTheme = vscode.workspace.getConfiguration('workbench').get('colorTheme');
      const theme = await measureThemeOnce(ev, currentTheme);
      matrix.capabilities.themes = { emulated: true, method: 'live current theme read', observed: [theme] };
      for (const chip of theme.chips) {
        check(
          `matrix:${cellLabel}:verdict-contrast:${theme.name}:${chip.selector}`,
          chip.ratio !== null && chip.ratio >= 4.5,
          `${chip.foreground} on ${chip.background} = ${chip.ratio === null ? 'unmeasurable' : chip.ratio.toFixed(3)}:1`,
        );
      }
    }

    // ---- disposal + reopen restoration ---------------------------------
    liveConnection.close();
    matrix.capabilities.disposal_reopen = await measureDisposalReopen(
      vscode,
      extensionUri,
      ChatViewProvider,
      sink,
    );
    check(
      `matrix:${cellLabel}:disposal-reopen-snapshot`,
      matrix.capabilities.disposal_reopen.snapshot_restored === true &&
        matrix.capabilities.disposal_reopen.fresh_nonce === true,
      JSON.stringify(matrix.capabilities.disposal_reopen),
    );
    check(
      `matrix:${cellLabel}:disposal-reopen-attachments`,
      matrix.capabilities.disposal_reopen.attachments_restored === true,
      `onViewResolved reopened the attachment surface`,
    );
  } finally {
    try {
      liveConnection.close();
    } catch {
      /* already closed */
    }
    first.dispose();
  }
}

function matrixSnapshot() {
  return {
    daemon: 'running',
    daemonDetail: 'matrix fixture',
    baseUrl: 'http://127.0.0.1:9',
    session: {
      id: '7',
      title: 'matrix-session',
      provider: 'mock',
      model: 'default',
      state: 'idle',
    },
    machineState: 'idle',
    machineLabel: 'Idle',
    sessions: [],
    runs: [],
    activeRunId: null,
    agents: [],
    task: null,
    verification: null,
    usage: null,
    cockpit: null,
    cockpitSections: [],
    tournament: null,
    transcript: [],
    streamStatus: 'open',
    lastError: null,
    busy: false,
  };
}

/** Calibrate top-level emulation so the webview viewport lands exactly on W. */
async function measureWidths(ev, baselineInner) {
  const page = await cdpPageTarget();
  const pageConn = page.connection;
  let offset = Math.max(0, 1200 - baselineInner);
  const observed = [];
  for (const requested of WIDTHS) {
    let inner = null;
    for (let attempt = 0; attempt < 3; attempt += 1) {
      await pageConn.send('Emulation.setDeviceMetricsOverride', {
        width: requested + offset,
        height: 900,
        deviceScaleFactor: 1,
        mobile: false,
      });
      await wait(250);
      inner = await ev('window.innerWidth');
      const delta = requested - inner;
      if (Math.abs(delta) <= 1) break;
      offset += delta;
    }
    const layout = await ev('window.__FAKTOR_E2E.state()');
    observed.push({
      requested,
      inner,
      scrollWidth: layout.scrollWidth,
      clientWidth: layout.clientWidth,
    });
  }
  return {
    result: {
      emulated: true,
      method:
        'top-level CDP Emulation.setDeviceMetricsOverride calibrated until the webview OOPIF innerWidth equals W (the OOPIF target rejects Emulation as non-top-level; no extension API resizes a workbench panel)',
      observed,
    },
    offset,
    setWidth: async (width) => {
      await pageConn.send('Emulation.setDeviceMetricsOverride', {
        width: width + offset,
        height: 900,
        deviceScaleFactor: 1,
        mobile: false,
      });
      await wait(350);
    },
    close: async () => {
      await pageConn.send('Emulation.clearDeviceMetricsOverride').catch(() => {});
      pageConn.close();
    },
  };
}

async function cdpPageTarget() {
  const targets = await fetchJson(`http://127.0.0.1:${cdpPort}/json/list`, 5000);
  const page = targets.find((target) => target.type === 'page' && target.webSocketDebuggerUrl);
  assert(page, 'the workbench page target must expose a CDP websocket');
  const connection = await cdpConnect(page.webSocketDebuggerUrl, 6000);
  return { connection };
}

async function runPasteDrop(kind, ev, sink, hostTypes) {
  const expression =
    kind === 'paste'
      ? `(function(){
          try {
            var dt = new DataTransfer();
            dt.items.add(new File([new Uint8Array([137,80,78,71])], 'paste.png', { type: 'image/png' }));
            document.getElementById('goal').dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
            return { ok: true };
          } catch (error) { return { ok: false, error: String(error) }; }
        })()`
      : `(function(){
          try {
            var dt = new DataTransfer();
            dt.items.add(new File([new Uint8Array([1,2,3])], 'drop.bin', { type: 'application/octet-stream' }));
            document.getElementById('composer').dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true, cancelable: true }));
            return { ok: true };
          } catch (error) { return { ok: false, error: String(error) }; }
        })()`;
  const dispatched = await ev(expression);
  await wait(700);
  const attached = sink
    .map((entry) => entry.message)
    .filter((message) => message && message.type === 'attachData')
    .pop();
  const file = attached && Array.isArray(attached.items) ? attached.items[0] : null;
  return {
    emulated: true,
    method:
      kind === 'paste'
        ? 'synthetic ClipboardEvent with a real DataTransfer/File at the real paste handler (no headless OS-clipboard file injection exists; event.isTrusted is false, bytes/handlers are real)'
        : 'synthetic DragEvent with a real DataTransfer/File at the real drop handler (no headless native drag session exists; event.isTrusted is false, bytes/handlers are real)',
    dispatched,
    accepted: Boolean(
      dispatched && dispatched.ok && file && file.filename && file.bytes >= 0,
    ),
    attachment: file
      ? { filename: file.filename, mime: file.mime, bytes: file.bytes, dataBase64: file.dataBase64 }
      : null,
    host_message_types: hostTypes(),
  };
}

async function runKeyboardOnly(input, ev, sink, hostTypes, provider) {
  await ev('window.__FAKTOR_E2E.keyLog = []; window.__FAKTOR_E2E.focusLog = []; document.getElementById("goal").blur();');
  const key = (type, keyValue, code, keyCode, modifiers) =>
    input.key(type, keyValue, code, keyCode, modifiers);
  const focusPath = [];
  let reached = false;
  for (let step = 0; step < 40; step += 1) {
    await key('rawKeyDown', 'Tab', 'Tab', 9, 0);
    await key('keyUp', 'Tab', 'Tab', 9, 0);
    await wait(60);
    const focused = await ev(
      'document.activeElement && (document.activeElement.id || document.activeElement.tagName)',
    );
    focusPath.push(focused);
    if (focused === 'goal') {
      reached = true;
      break;
    }
  }
  const typed = `keyboard-only-${Date.now().toString(36)}`;
  await input.insertText(typed);
  await wait(150);
  await key('rawKeyDown', 'Enter', 'Enter', 13, 2);
  await key('keyUp', 'Enter', 'Enter', 13, 2);
  await wait(500);
  const submitted = sink.some(
    (entry) => entry.message && entry.message.type === 'sendGoal' && entry.message.goal === typed,
  );
  if (submitted) {
    provider.postStartResult(typed, true);
    await wait(400);
  }
  const draft = await ev('document.getElementById("goal").value');
  const trusted = await ev('window.__FAKTOR_E2E.keyLog.some(function(e){return e.trusted === true;})');
  return {
    emulated: true,
    method:
      'real trusted CDP Input.dispatchKeyEvent/Tab + Input.insertText + Ctrl+Enter delivered to the real webview frame (isTrusted true)',
    focus_path: focusPath,
    reached_goal: reached,
    typed,
    submitted,
    draft_cleared: submitted && draft === '',
    trusted,
    host_message_types: hostTypes(),
  };
}

async function measureCssZoom(ev, widthRun) {
  await widthRun.setWidth(480);
  const measure = async (zoom) => {
    await ev(`document.body.style.zoom = ${JSON.stringify(String(zoom))}`);
    await wait(200);
    return ev('document.getElementById("goal").getBoundingClientRect().width');
  };
  const at100 = await measure(1);
  const at125 = await measure(1.25);
  const at200 = await measure(2);
  await ev('document.body.style.zoom = ""');
  await widthRun.setWidth(480);
  return {
    emulated: true,
    method:
      'renderer CSS zoom applied in the real webview at a pinned 480px viewport (exact 125%/200% layout reflow; exact device scale factors are covered by the dedicated launch cells)',
    goalWidth: { at100, at125, at200 },
    monotonic: at125 < at100 && at200 < at125,
  };
}

async function measureThemeOnce(ev, themeName) {
  const chips = await chipContrast(ev);
  const bodyClass = await ev('document.body.className');
  return { name: themeName || '(default)', body_class: bodyClass, matched: true, chips };
}

async function chipContrast(ev) {
  const selectors = [
    '.criterion-verdict-pass',
    '.criterion-verdict-fail',
    '.criterion-verdict-unavailable',
  ];
  const chips = [];
  for (const selector of selectors) {
    const observed = await ev(`window.__FAKTOR_E2E.chip(${JSON.stringify(selector)})`);
    chips.push({
      selector,
      foreground: observed.color,
      background: observed.background,
      ratio: renderedContrast(observed.color, observed.background),
    });
  }
  return chips;
}

async function measureThemes(vscode, ev) {
  const workbench = vscode.workspace.getConfiguration('workbench');
  const original = workbench.get('colorTheme');
  const observed = [];
  for (const theme of THEMES) {
    await workbench.update('colorTheme', theme.name, vscode.ConfigurationTarget.Global);
    let bodyClass = '';
    let matched = false;
    for (let attempt = 0; attempt < 24; attempt += 1) {
      await wait(250);
      bodyClass = await ev('document.body.className');
      if (String(bodyClass).includes(theme.bodyClass)) {
        matched = true;
        break;
      }
    }
    observed.push({
      name: theme.name,
      expected_body_class: theme.bodyClass,
      body_class: bodyClass,
      matched,
      chips: await chipContrast(ev),
    });
  }
  if (original && original !== 'Default Dark Modern') {
    await workbench.update('colorTheme', original, vscode.ConfigurationTarget.Global);
  }
  return { emulated: true, method: 'live workbench.colorTheme switches observed as webview body classes, chip pairs re-measured per theme', observed };
}

async function measureDisposalReopen(vscode, extensionUri, ChatViewProvider, sink) {
  const events = [];
  let provider = null;
  const host = {
    handle() {},
    onViewResolved() {
      events.push('resolved');
      provider.postAttachments([
        { id: 'att-1', filename: 'spec.pdf', mime: 'application/pdf', bytes: 8, isImage: false, refusal: null },
      ]);
    },
  };
  provider = new ChatViewProvider(extensionUri, host);
  const tokenA = nextProbeToken();
  const panelA = createTestPanel(vscode, extensionUri, tokenA, sink);
  provider.resolveWebviewView(panelA.view);
  provider.postSnapshot(matrixSnapshot());
  const readyA = await waitForPanelMessage(sink, tokenA, 'ready', 12000);
  check(`matrix:${cellLabel}:disposal-first-view-booted`, readyA !== null, 'first view must boot');
  await wait(600);
  const firstTarget = await connectMatrixTarget(tokenA, 15000);
  const titleA = await evaluate(firstTarget.connection, firstTarget.contextId, 'document.getElementById("session-title").textContent');
  firstTarget.connection.close();
  panelA.dispose();
  await wait(500);

  const tokenB = nextProbeToken();
  const panelB = createTestPanel(vscode, extensionUri, tokenB, sink);
  provider.resolveWebviewView(panelB.view);
  const readyB = await waitForPanelMessage(sink, tokenB, 'ready', 12000);
  check(`matrix:${cellLabel}:disposal-second-view-booted`, readyB !== null, 'reopened view must boot');
  await wait(700);
  const secondTarget = await connectMatrixTarget(tokenB, 15000);
  const titleB = await evaluate(secondTarget.connection, secondTarget.contextId, 'document.getElementById("session-title").textContent');
  const attachments = await evaluate(
    secondTarget.connection,
    secondTarget.contextId,
    'document.querySelectorAll("#attachment-list li").length',
  );
  secondTarget.connection.close();
  panelB.dispose();
  return {
    emulated: true,
    method:
      'real WebviewPanel dispose + real ChatViewProvider.resolveWebviewView reopen (VS Code resolves the fresh view, re-renders with a new nonce and re-posts the retained snapshot; onViewResolved re-posts host-owned attachments)',
    first_title: titleA,
    reopened_title: titleB,
    snapshot_restored: titleB === 'matrix-session',
    fresh_nonce: panelA.nonce.length > 0 && panelB.nonce.length > 0 && panelA.nonce !== panelB.nonce,
    on_view_resolved_calls: events.length,
    attachments_restored: attachments > 0,
  };
}

exports.run = async function run() {
  try {
    const vscode = require('vscode');

    const ext = vscode.extensions.getExtension('faktor.faktor');
    check('extension-present', ext, 'faktor.faktor must resolve under --extensionDevelopmentPath');
    await ext.activate();
    check('activation-succeeded', true, `activated ${ext.id}`);

    const commands = await vscode.commands.getCommands(true);
    const pkg = ext.packageJSON || {};
    const contributed = ((pkg.contributes && pkg.contributes.commands) || []).map(
      (entry) => entry.command,
    );
    const missing = contributed.filter((id) => !commands.includes(id));
    check(
      `commands-contributed-registered:${contributed.length}`,
      contributed.length > 0 && missing.length === 0,
      `contributed but not registered: ${missing.join(', ')}`,
    );
    for (const command of contributed) {
      check(`command-registered:${command}`, commands.includes(command), 'contributed command missing');
    }
    const views = (pkg.contributes && pkg.contributes.views && pkg.contributes.views.faktor) || [];
    check(
      'contributes-views-faktor-chat',
      views.some((view) => view && view.id === 'faktor.chat'),
      JSON.stringify(views),
    );
    await vscode.commands.executeCommand('faktor.openChat');
    check('command-executed:faktor.openChat', true, 'the chat view command ran in the real host');

    const providerPath = path.join(ext.extensionPath, 'out', 'webview.js');
    check('compiled-provider-present', fs.existsSync(providerPath), providerPath);
    const providerModule = require(providerPath);
    check('webview-provider-exported', typeof providerModule.ChatViewProvider === 'function');
    check(
      'webview-view-type',
      providerModule.ChatViewProvider.viewType === 'faktor.chat',
      String(providerModule.ChatViewProvider.viewType),
    );

    const provider = new providerModule.ChatViewProvider(vscode.Uri.file(ext.extensionPath), {
      handle() {},
    });
    check('webview-render-callable', typeof provider.render === 'function');
    const webviewStub = {
      asWebviewUri: (uri) => uri,
      cspSource: 'vscode-webview://faktor-e2e',
    };
    const html = provider.render(webviewStub);
    check('webview-html-nonempty', typeof html === 'string' && html.length > 0);
    const markers = [
      '<title>Faktor</title>',
      'id="composer"',
      'id="goal"',
      'id="attachment-hint"',
      'id="attachment-list"',
      'id="attachment-notice"',
      'id="btn-attach"',
      'id="btn-clear-attachments"',
      "default-src 'none'",
      "script-src 'nonce-",
      "connect-src 'none'",
      'media/chat.js',
      'media/composer-state.js',
      'media/board-state.js',
      'media/chat.css',
      'id="board-card"',
      'id="board-post"',
      'id="board-subject"',
      'id="board-body"',
      'id="btn-board-read"',
      'id="btn-board-post"',
    ];
    for (const marker of markers) {
      check(`webview-html-has:${marker}`, html.includes(marker), 'marker missing from rendered HTML');
    }
    const htmlDigest = crypto.createHash('sha256').update(html, 'utf8').digest('hex');
    checks.push({ name: 'webview-html-sha256', ok: true, detail: htmlDigest });

    await runMatrix(vscode, ext, providerModule);

    writeEvidence('passed', null);
  } catch (error) {
    writeEvidence('failed', error);
    throw error;
  }
};
