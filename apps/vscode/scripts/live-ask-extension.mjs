#!/usr/bin/env node
// LIVE Ask through the INSTALLED Faktor VS Code extension (activity-bar sidebar).
//
// A clean VS Code profile launches the installed `faktor.faktor` extension on a
// stable throwaway workspace and a throwaway daemon data dir whose
// faktor-plus.json is copied from ~/.faktor-live/data (DeepSeek via
// provider_env_file; the key is never read or printed here).
//
// The script clicks the Faktor activity-bar view, waits for the verified
// daemon auto-start, types the Ask turn into the real sidebar composer and
// presses Send, then waits for the real durable assistant reply and captures
// the DOM text, daemon process/port and a screenshot as evidence.

import { spawn, spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import {
  copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync,
  rmSync, statSync, writeFileSync,
} from 'node:fs';
import { createServer } from 'node:net';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..', '..');
const outDir = resolve(process.argv[2] || join(repoRoot, 'docs/specs/live-evidence/ask-fetch-fix'));
const liveHome = process.env.FAKTOR_LIVE_HOME || join(homedir(), '.faktor-live');
const codeBin = process.env.VSCODE_CLI || '/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code';
const extensionsDir = process.env.FAKTOR_VSCODE_EXTENSIONS_DIR || join(homedir(), '.vscode', 'extensions');
const installedExtension = join(extensionsDir, 'faktor.faktor-0.1.0');
const timeoutMs = Number(process.env.FAKTOR_LIVE_TIMEOUT_MS || '240000');
const marker = process.env.FAKTOR_LIVE_MARKER || `ASK-LIVE-${randomUUID().slice(0, 8).toUpperCase()}`;
const prompt = `Reply with exactly this token and nothing else: ${marker}`;

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const assert = (condition, message) => {
  if (!condition) throw new Error(message);
};

async function freePort() {
  return await new Promise((done, fail) => {
    const server = createServer();
    server.once('error', fail);
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      server.close(() => done(port));
    });
  });
}

async function fetchJson(url, timeout = 5000) {
  const response = await fetch(url, { signal: AbortSignal.timeout(timeout) });
  if (!response.ok) throw new Error(`HTTP ${response.status} for ${url}`);
  return response.json();
}

async function cdpConnect(url, timeout = 5000) {
  const ws = new WebSocket(url);
  await new Promise((done, fail) => {
    const timer = setTimeout(() => fail(new Error(`CDP socket open timeout: ${url}`)), timeout);
    ws.onopen = () => { clearTimeout(timer); done(); };
    ws.onerror = () => { clearTimeout(timer); fail(new Error(`CDP socket error: ${url}`)); };
  });
  let nextId = 0;
  const pending = new Map();
  const events = [];
  ws.onmessage = (event) => {
    let message;
    try { message = JSON.parse(event.data); } catch { return; }
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
  const send = (method, params) => new Promise((done, fail) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      fail(new Error(`${method} timeout`));
    }, timeout);
    pending.set(id, { resolve: done, reject: fail, timer, method });
    ws.send(JSON.stringify({ id, method, params: params || {} }));
  });
  return { send, events, close: () => ws.close() };
}

function valueOf(result) {
  if (result && result.exceptionDetails) {
    throw new Error(`evaluate failed: ${JSON.stringify(result.exceptionDetails).slice(0, 300)}`);
  }
  return result && result.result ? result.result.value : undefined;
}

async function evalIn(connection, expression, contextId) {
  const params = { expression, returnByValue: true, awaitPromise: true };
  if (contextId !== undefined) params.contextId = contextId;
  return valueOf(await connection.send('Runtime.evaluate', params));
}

async function pageTarget(port, deadline) {
  let last = 'no page target';
  while (Date.now() < deadline) {
    try {
      const targets = await fetchJson(`http://127.0.0.1:${port}/json/list`);
      const page = targets.find((target) => target.type === 'page' && target.webSocketDebuggerUrl);
      if (page) return page;
    } catch (error) {
      last = String(error && error.message);
    }
    await sleep(300);
  }
  throw new Error(`VS Code page target never appeared (${last})`);
}

/** Find the live Faktor sidebar webview by its composer DOM in any context. */
async function webviewTarget(port, deadline) {
  let last = 'no iframe target';
  while (Date.now() < deadline) {
    let targets = [];
    try {
      targets = await fetchJson(`http://127.0.0.1:${port}/json/list`);
    } catch (error) {
      last = String(error && error.message);
      await sleep(300);
      continue;
    }
    for (const target of targets.filter((entry) => entry.type === 'iframe' && entry.webSocketDebuggerUrl)) {
      let connection = null;
      try {
        connection = await cdpConnect(target.webSocketDebuggerUrl, 4000);
        await connection.send('Runtime.enable');
        await sleep(250);
        const contexts = connection.events
          .filter((event) => event.method === 'Runtime.executionContextCreated')
          .map((event) => event.params.context);
        for (const context of contexts) {
          try {
            const found = await evalIn(
              connection,
              "!!document.getElementById('goal') && !!document.getElementById('composer')",
              context.id,
            );
            if (found) return { connection, contextId: context.id, url: target.url };
          } catch {
            /* context detached */
          }
        }
        connection.close();
      } catch (error) {
        last = String(error && error.message);
        if (connection) { try { connection.close(); } catch { /* closed */ } }
      }
    }
    await sleep(350);
  }
  throw new Error(`the Faktor sidebar webview never rendered (${last})`);
}

function findDaemon(dataDir) {
  const result = spawnSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
  const lines = (result.stdout || '').split('\n').map((line) => line.trim()).filter(Boolean);
  for (const line of lines) {
    const match = /^(\d+)\s+(.*)$/.exec(line);
    if (!match) continue;
    if (match[2].includes('faktor') && match[2].includes('serve') && match[2].includes(dataDir)) {
      return { pid: Number(match[1]), command: match[2] };
    }
  }
  return null;
}

function listeningPort(pid) {
  const result = spawnSync('lsof', ['-nP', '-a', '-p', String(pid), '-iTCP', '-sTCP:LISTEN'], { encoding: 'utf8' });
  const match = /127\.0\.0\.1:(\d+)\s+\(LISTEN\)/.exec(result.stdout || '');
  return match ? Number(match[1]) : null;
}

function sha256(file) {
  const result = spawnSync('shasum', ['-a', '256', file], { encoding: 'utf8' });
  return (result.stdout || '').trim().split(/\s+/)[0] || null;
}

mkdirSync(outDir, { recursive: true });
assert(existsSync(installedExtension), `installed extension missing: ${installedExtension}`);
const installedPackage = JSON.parse(readFileSync(join(installedExtension, 'package.json'), 'utf8'));
assert(installedPackage.name === 'faktor', 'the installed extension is faktor');
assert(existsSync(join(installedExtension, 'out', 'askRecovery.js')), 'installed extension carries askRecovery.js');
const binary = join(liveHome, 'bin', 'faktor');
assert(existsSync(binary), `missing live daemon: ${binary}`);
const liveDataConfig = join(liveHome, 'data', 'faktor-plus.json');
assert(existsSync(liveDataConfig), `missing live provider config: ${liveDataConfig}`);

const runDir = mkdtempSync(join(tmpdir(), 'faktor-live-ext-'));
const workspace = join(runDir, 'workspace');
const userData = join(runDir, 'user-data');
const dataDir = join(runDir, 'data');
mkdirSync(workspace, { recursive: true });
mkdirSync(join(userData, 'User'), { recursive: true });
mkdirSync(dataDir, { recursive: true });
copyFileSync(liveDataConfig, join(dataDir, 'faktor-plus.json'));
writeFileSync(join(workspace, 'README.md'), '# Live Ask workspace\n\nStable throwaway workspace for the live Ask evidence run.\n');
writeFileSync(join(userData, 'User', 'settings.json'), `${JSON.stringify({
  'security.workspace.trust.enabled': false,
  'telemetry.telemetryLevel': 'off',
  'workbench.startupEditor': 'none',
  'workbench.tips.enabled': false,
  'update.mode': 'none',
  'extensions.autoCheckUpdates': false,
  'extensions.autoUpdate': false,
  'faktor.installRoot': join(liveHome, 'bin'),
  'faktor.binaryPath': binary,
  'faktor.dataDir': dataDir,
  'faktor.startupTimeoutMs': 90000,
  'faktor.defaultProvider': 'deepseek',
  'faktor.defaultModel': 'deepseek-chat',
}, null, 2)}\n`);

const cdpPort = await freePort();
const evidence = {
  schema: 'faktor-live-ask-extension/v1',
  capturedAt: new Date().toISOString(),
  repoRoot,
  installedExtension: {
    path: installedExtension,
    name: installedPackage.name,
    version: installedPackage.version,
    askRecoverySha256: sha256(join(installedExtension, 'out', 'askRecovery.js')),
  },
  daemon: {
    binaryPath: binary,
    binarySha256: sha256(binary),
    checksumFile: readFileSync(`${binary}.sha256`, 'utf8').trim(),
    dataDir,
    configCopiedFrom: liveDataConfig,
    provider: 'deepseek',
    model: 'deepseek-chat',
  },
  workspace,
  profile: userData,
  cdpPort,
  marker,
  ok: false,
  steps: {},
  assistant: null,
  error: null,
};

let appPid = null;
let page = null;
let webview = null;
try {
  const child = spawn(codeBin, [
    `--user-data-dir=${userData}`,
    `--extensions-dir=${extensionsDir}`,
    `--remote-debugging-port=${cdpPort}`,
    '--remote-allow-origins=*',
    '--new-window',
    workspace,
  ], { detached: true, stdio: 'ignore' });
  appPid = child.pid;
  child.unref();

  const started = Date.now();
  const target = await pageTarget(cdpPort, Date.now() + 60000);
  page = await cdpConnect(target.webSocketDebuggerUrl, 10000);
  await page.send('Runtime.enable');
  await page.send('Page.enable');

  // A fresh profile shows VS Code's multi-step welcome/sign-in onboarding
  // over the workbench. Walk it through (Continue / close) so the Faktor
  // activity-bar view is reachable; the final evidence screenshot hides any
  // remainder of that IDE-owned overlay.
  const welcomeDeadline = Date.now() + 30000;
  while (Date.now() < welcomeDeadline) {
    const dismissed = await evalIn(page, `(() => {
      const dialogs = Array.from(document.querySelectorAll('.monaco-dialog-box, .dialog-box'));
      const dialog = dialogs.find((node) => node.offsetParent !== null);
      const close = dialog && Array.from(dialog.querySelectorAll('.codicon-close, [aria-label="Close"], [aria-label="close"]')).find((node) => node.offsetParent !== null);
      if (close) { close.click(); return 'closed'; }
      const hit = Array.from((dialog || document).querySelectorAll('a, button')).find((node) => node.offsetParent !== null && /^(Continue|Continue without Signing In|Done|Finish|Next)$/i.test((node.textContent || '').trim()));
      if (hit) { hit.click(); return 'continued'; }
      if (dialog) return null;
      return 'none';
    })()`).catch(() => null);
    if (dismissed === 'none') break;
    await sleep(500);
  }
  await sleep(300);

  // Wait for the workbench shell, then click the Faktor activity-bar view.
  const shellDeadline = Date.now() + 60000;
  let clicked = null;
  while (Date.now() < shellDeadline) {
    clicked = await evalIn(page, `(() => {
      const labels = Array.from(document.querySelectorAll('.activitybar .action-label, .activitybar [role="tab"], .activitybar li'));
      const el = labels.find((node) => /faktor/i.test(node.getAttribute('aria-label') || node.title || node.textContent || ''));
      if (!el) return null;
      el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, view: window }));
      return el.getAttribute('aria-label') || el.title || el.textContent;
    })()`).catch(() => null);
    if (clicked !== null) break;
    await sleep(500);
  }
  assert(clicked !== null, 'the Faktor activity-bar entry was not found');
  evidence.steps.openSidebar = { label: String(clicked).slice(0, 80), atMs: Date.now() - started };

  // Clicking the view resolves + activates it; the extension auto-starts the
  // verified installed daemon from the machine-scoped settings.
  webview = await webviewTarget(cdpPort, Date.now() + 90000);
  const ev = (expression) => evalIn(webview.connection, expression, webview.contextId);
  evidence.steps.webview = { url: webview.url, atMs: Date.now() - started };

  // The profile starts with NO daemon for this data dir: observing the
  // process proves the extension auto-started the verified installed binary.
  const spawnDeadline = Date.now() + 90000;
  let firstSeen = null;
  while (Date.now() < spawnDeadline) {
    firstSeen = findDaemon(dataDir);
    if (firstSeen !== null) break;
    await sleep(300);
  }
  assert(firstSeen !== null, 'the verified daemon did not auto-start');
  const firstSeenAtMs = Date.now() - started;

  // Then wait until the daemon's loopback port is bound and the sidebar has
  // switched from its initial idle label (never a blind send).
  const daemonDeadline = Date.now() + 90000;
  let preState = null;
  let port = null;
  while (Date.now() < daemonDeadline) {
    preState = await ev(`(() => ({
      body: document.body.innerText.slice(0, 2500),
      daemon: (document.getElementById('daemon-text') || {}).textContent || '',
      dot: (document.getElementById('daemon-dot') || {}).className || '',
      composer: (document.getElementById('composer-state-label') || {}).textContent || '',
      sendDisabled: !!(document.getElementById('btn-send') || {}).disabled,
    }))()`);
    port = listeningPort(firstSeen.pid);
    if (port !== null && preState && preState.daemon !== 'Ready') break;
    await sleep(300);
  }
  evidence.steps.autoStart = {
    firstSeenAtMs,
    atMs: Date.now() - started,
    pid: firstSeen.pid,
    listeningPort: port,
    daemonText: preState && preState.daemon,
    composerState: preState && preState.composer,
    sendDisabled: preState && preState.sendDisabled,
  };

  // Type one Ask turn through the REAL sidebar composer and press Send.
  const entered = await ev(`(() => {
    const goal = document.getElementById('goal');
    const send = document.getElementById('btn-send');
    if (!goal || !send) return 'missing-composer';
    goal.focus();
    goal.value = ${JSON.stringify(prompt)};
    goal.dispatchEvent(new Event('input', { bubbles: true }));
    send.click();
    return 'sent';
  })()`);
  assert(entered === 'sent', `the sidebar composer did not send: ${entered}`);
  evidence.steps.ask = { prompt, sentAtMs: Date.now() - started };

  // The user row echoes the prompt; require a DIFFERENT assistant row that
  // carries the marker, so an echo can never be mistaken for a reply.
  const replyDeadline = Date.now() + timeoutMs;
  let body = '';
  let assistantText = null;
  while (Date.now() < replyDeadline) {
    body = await ev("document.body.innerText");
    const rows = await ev(`(() => Array.from(document.querySelectorAll('.entry.entry-assistant')).map((node) => node.innerText))()`);
    const hit = Array.isArray(rows) ? rows.find((row) => row && row.includes(marker)) : undefined;
    if (hit) { assistantText = hit; break; }
    await sleep(1000);
  }
  assert(assistantText !== null, `no assistant reply carrying ${marker}; sidebar text tail: ${body.slice(-1200)}`);
  evidence.assistant = {
    marker,
    text: String(assistantText).slice(0, 4000),
    transcriptAssistantRows: (await ev("document.querySelectorAll('.entry.entry-assistant').length")),
    deliveredAtMs: Date.now() - started,
  };

  const postState = await ev(`(() => ({
    daemon: (document.getElementById('daemon-text') || {}).textContent || '',
    composer: (document.getElementById('composer-state-label') || {}).textContent || '',
    bodyTail: document.body.innerText.slice(-4000),
  }))()`);
  evidence.steps.postTurn = postState;

  // Let the final assistant row paint before capturing the installed sidebar.
  // The IDE-owned onboarding overlay is hidden only for the capture; it is
  // not part of the Faktor surface.
  await sleep(750);
  evidence.domDiagnostics = await evalIn(page, `(() => ({
    iframes: Array.from(document.querySelectorAll('iframe')).map((node) => {
      const r = node.getBoundingClientRect();
      return { id: node.id, src: String(node.src || '').slice(0, 220), x: r.x, y: r.y, width: r.width, height: r.height, visible: node.offsetParent !== null };
    }),
    overlays: Array.from(document.querySelectorAll('[class*=dialog],[class*=welcome],[class*=onboard],[class*=gettingStarted]')).filter((node) => node.offsetParent !== null).slice(0, 20).map((node) => String(node.className)),
  }))()`).catch(() => null);
  await evalIn(page, `(() => {
    const style = document.createElement('style');
    style.id = 'faktor-live-evidence-overlay-hide';
    style.textContent = '.monaco-dialog-box,.monaco-dialog-modal-block,.dialog-box,.gettingStartedContainer,.welcome-view,[class*="onboarding-a-"],.chat-welcome-view-container,.chat-welcome-view{display:none!important}';
    document.head.appendChild(style);
  })()`).catch(() => null);
  await sleep(250);
  let webviewId = null;
  try { webviewId = new URL(webview.url).searchParams.get('id'); } catch { /* keep null */ }
  const rect = await evalIn(page, `(() => {
    const frames = Array.from(document.querySelectorAll('iframe'));
    const wanted = ${JSON.stringify(webviewId)};
    const frame = (wanted && frames.find((node) => decodeURIComponent(String(node.src || '')).includes(wanted))) || null;
    if (!frame) return null;
    const r = frame.getBoundingClientRect();
    return { x: Math.max(0, r.x), y: Math.max(0, r.y), width: r.width, height: r.height };
  })()`).catch(() => null);
  const shotPath = join(outDir, 'extension-live.png');
  const workbenchPath = join(outDir, 'extension-live-workbench.png');
  let clipped = false;
  try {
    const shot = await webview.connection.send('Page.captureScreenshot', { format: 'png' });
    writeFileSync(shotPath, Buffer.from(shot.data, 'base64'));
    clipped = true;
    evidence.webviewScreenshot = shotPath;
  } catch (error) {
    evidence.webviewScreenshotError = String(error && error.message ? error.message : error);
  }
  if (!clipped && rect && rect.width > 50 && rect.height > 50) {
    try {
      const shot = await page.send('Page.captureScreenshot', {
        format: 'png',
        clip: { x: rect.x, y: rect.y, width: rect.width, height: rect.height, scale: 2 },
      });
      writeFileSync(shotPath, Buffer.from(shot.data, 'base64'));
      clipped = true;
    } catch (error) {
      evidence.clipScreenshotError = String(error && error.message ? error.message : error);
    }
  }
  const shot = await page.send('Page.captureScreenshot', { format: 'png', captureBeyondViewport: false });
  writeFileSync(workbenchPath, Buffer.from(shot.data, 'base64'));
  if (!clipped) writeFileSync(shotPath, Buffer.from(shot.data, 'base64'));
  evidence.screenshot = shotPath;
  evidence.workbenchScreenshot = workbenchPath;
  evidence.sidebarRect = rect;
  evidence.ok = true;
} catch (error) {
  evidence.error = {
    name: error && error.name ? error.name : 'Error',
    message: error && error.message ? error.message : String(error),
  };
  throw error;
} finally {
  if (webview) { try { webview.connection.close(); } catch { /* closed */ } }
  if (page) { try { page.close(); } catch { /* closed */ } }
  try {
    const logRoot = join(userData, 'logs');
    if (existsSync(logRoot)) {
      const files = [];
      const walk = (dir) => {
        for (const name of readdirSync(dir)) {
          const full = join(dir, name);
          if (statSync(full).isDirectory()) walk(full);
          else if (name.endsWith('.log')) files.push(full);
        }
      };
      walk(logRoot);
      evidence.logs = files.slice(-20).map((file) => ({
        file: file.replace(userData, '<profile>'),
        tail: readFileSync(file, 'utf8').split('\n').slice(-12).join('\n').slice(0, 2000),
      }));
    }
  } catch { /* diagnostics only */ }
  if (appPid !== null) {
    spawnSync('pkill', ['-f', userData]);
    await sleep(1500);
    spawnSync('pkill', ['-9', '-f', userData]);
    await sleep(500);
  }
  const daemon = findDaemon(dataDir);
  if (daemon !== null) {
    try { process.kill(daemon.pid, 'SIGTERM'); } catch { /* gone */ }
  }
  writeFileSync(join(outDir, 'extension-live.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  if (evidence.ok) rmSync(runDir, { recursive: true, force: true });
}

console.log(JSON.stringify({ ok: evidence.ok, assistant: evidence.assistant, out: join(outDir, 'extension-live.json') }));
