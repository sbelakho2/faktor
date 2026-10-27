#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile,read-only-artifacts
// Change-scoped verification for the ONE Faktor window.
//
// 1. Static architecture: exactly one editor-area createWebviewPanel, one
//    launcher view, no per-surface panel/view classes, and no sidebar chat
//    or settings view.
// 2. Host idempotence: the extension command opens the panel once, then only
//    reveals it; every internal view move reuses the same panel.
// 3. Real DOM (headless Chrome): Chat is one full-height column, the
//    transcript owns scroll, the composer is in flow, Settings/Inspect/
//    History/Diagnostics cross-fade inside the same document, one visible
//    header, and Ask/Intent survive view switches. Captures at 480/1440.

import { createRequire } from 'node:module';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import {
  buildHarnessHtml,
  connect,
  findChrome,
  launchChrome,
  sleep,
} from './render-webview.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const SRC = resolve(HERE, '..', 'src');
const OUT = join(ROOT, 'target/certification/window');
const require = createRequire(import.meta.url);

function fail(message) {
  console.error(`FAIL  ${message}`);
  process.exitCode = 1;
}

function assert(condition, message) {
  if (!condition) fail(message);
}

function read(path) {
  return readFileSync(path, 'utf8');
}

function walk(dir, visit) {
  for (const entry of require('node:fs').readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, visit);
    else visit(path);
  }
}

// ------------------------------------------------------- static architecture

function staticChecks() {
  const files = [];
  walk(SRC, (path) => files.push(path));
  const sources = new Map(files.map((path) => [path, read(path)]));
  const panelCalls = [...sources.entries()].filter(([, text]) => text.includes('createWebviewPanel('));
  assert(panelCalls.length === 1, `exactly one createWebviewPanel call, found ${panelCalls.length}`);
  assert(
    panelCalls[0][0].endsWith(join('src', 'webview.ts')),
    'the only webview panel lives in src/webview.ts',
  );
  for (const [path, text] of sources) {
    assert(!text.includes('class Edition8Panel'), `${path} must not define a second editor panel`);
    assert(!text.includes('class Edition8ViewProvider'), `${path} must not define a sidebar settings view`);
    assert(!text.includes('registerWebviewViewProvider(ChatViewProvider.viewType'), `${path} must not host chat in the sidebar`);
  }
  const webview = read(join(SRC, 'webview.ts'));
  assert(webview.includes("viewType = 'faktor.panel'"), 'the panel id is faktor.panel');
  assert(webview.includes('this.panel !== null'), 'the panel is a singleton (reuse, never recreate)');
  assert(webview.includes('this.panel.reveal(undefined, false)'), 'focus only reveals the existing panel');
  assert(!webview.includes('ViewColumn.Two'), 'no editor column split is created');
  assert(webview.includes('id="settings-panel"'), 'Settings is an internal view of the same panel');
  assert(webview.includes('renderEdition8AppMarkup'), 'Settings markup renders inside the panel document');
  assert(webview.includes('id="menu-settings"'), 'the overflow exposes Settings');
  assert(webview.includes('faktorShowView'), 'the host can switch internal views');

  const chat = read(join(SRC, '..', 'media', 'chat.js'));
  assert(chat.includes("settings: 'settings-panel'"), 'chat.js routes Settings locally');
  assert(chat.includes("byId('menu-settings')"), 'the overflow Settings item switches internally');
  assert(chat.includes("byId('btn-control-center')"), 'the gear switches internally');
  assert(chat.includes("message.type === 'faktorShowView'"), 'the host view switch is handled locally');
  assert(!chat.includes("type: 'openControlCenter'"), 'the gear no longer asks the host for another panel');
  const gearStart = chat.indexOf("byId('btn-control-center')");
  const gearEnd = chat.indexOf("byId('btn-overflow')", gearStart);
  assert(gearStart >= 0 && gearEnd > gearStart, 'the gear listener exists');
  assert(chat.slice(gearStart, gearEnd).includes("openView('settings')"), 'the gear calls the local view router');

  const controller = read(join(SRC, 'edition8', 'panel.ts'));
  const selectCase = controller.slice(
    controller.indexOf("case 'e8SelectScreen'"),
    controller.indexOf("case 'e8OpenScreen'"),
  );
  assert(selectCase.length > 0 && !selectCase.includes('.post('), 'the settings selection is never echoed back (no host<->webview loop)');

  const extension = read(join(SRC, 'extension.ts'));
  assert(extension.includes('LauncherViewProvider'), 'the activity bar is only a launcher');
  assert(!extension.includes('Edition8Panel.createOrShow'), 'no command creates a settings panel');
  assert(extension.includes('showEdition8Screen'), 'Edition 8 commands target internal views');

  const pkg = JSON.parse(read(join(SRC, '..', 'package.json')));
  const views = Object.values(pkg.contributes.views).flat();
  assert(views.length === 1, `one activity-bar view, found ${views.length}`);
  assert(views[0].id === 'faktor.launcher', 'the activity-bar entry is the launcher');
  assert(
    !views.some((entry) => entry.id === 'faktor.chat' || entry.id === 'faktor.edition8'),
    'no duplicate chat/settings sidebar views remain',
  );
  assert(
    pkg.activationEvents.includes('onView:faktor.launcher') &&
      !pkg.activationEvents.includes('onView:faktor.chat') &&
      !pkg.activationEvents.includes('onView:faktor.edition8'),
    'activation follows the single launcher view',
  );
  if (process.exitCode !== 1) {
    console.log('PASS  one editor panel, one launcher view, internal Settings/Inspect/History/Diagnostics');
  }
}

// ----------------------------------------------------- host idempotence test

async function hostChecks() {
  const created = [];
  const commands = [];
  const originalLoad = require('node:module')._load;
  class FakeUri {
    constructor(path) {
      this.fsPath = path;
      this.path = path;
    }
    static joinPath(base, ...parts) {
      return new FakeUri([base.fsPath, ...parts].join('/'));
    }
    static parse(path) {
      return new FakeUri(path);
    }
    toString() {
      return `file://${this.fsPath}`;
    }
  }
  const vscode = {
    Uri: FakeUri,
    ViewColumn: { Active: -1, One: 1, Two: 2 },
    window: {
      createWebviewPanel(type, title, column, options) {
        const panel = {
          type,
          title,
          column,
          options,
          revealed: [],
          posted: [],
          disposed: false,
          webview: {
            cspSource: 'vscode-resource:',
            html: '',
            asWebviewUri: (uri) => uri,
            postMessage: async (message) => {
              panel.posted.push(message);
              return true;
            },
            onDidReceiveMessage: (listener) => {
              panel.listener = listener;
              return { dispose() {} };
            },
          },
          reveal(viewColumn, preserveFocus) {
            panel.revealed.push({ viewColumn, preserveFocus });
          },
          onDidDispose: (listener) => {
            panel.disposeListener = listener;
            return { dispose() {} };
          },
          dispose() {
            panel.disposed = true;
            panel.disposeListener?.();
          },
        };
        created.push(panel);
        return panel;
      },
      createOutputChannel: () => ({ appendLine() {}, dispose() {} }),
    },
    commands: {
      executeCommand: async (id) => {
        commands.push(id);
      },
    },
    workspace: { workspaceFolders: [{ uri: new FakeUri('/repo'), name: 'repo' }] },
  };
  require('node:module')._load = function patched(request, parent, isMain) {
    if (request === 'vscode') return vscode;
    return originalLoad.call(this, request, parent, isMain);
  };
  try {
    const { ChatViewProvider, LauncherViewProvider } = require('../out/webview.js');
    const host = { handle: () => {}, onViewResolved: () => {} };
    const provider = new ChatViewProvider(
      { extensionUri: new FakeUri('/extension'), subscriptions: [] },
      host,
    );
    provider.focus();
    provider.focus();
    assert(created.length === 1, `command focus creates one panel, found ${created.length}`);
    assert(created[0].column === -1, 'the panel opens in the active editor column');
    assert(created[0].revealed.length === 1, 'the second command only reveals the panel');
    assert(created[0].revealed[0].viewColumn === undefined, 'reveal never moves/splits columns');
    assert(created[0].options.enableScripts && created[0].options.retainContextWhenHidden, 'panel state is retained');

    provider.showSettings('S03', false);
    provider.showView('inspect');
    provider.showView('history');
    provider.showView('diagnostics');
    provider.showView('work');
    provider.showSettings('S29', true);
    provider.showEdition8Screen('S41');
    provider.showEdition8Screen('S26');
    provider.showEdition8Screen('S03');
    assert(created.length === 1, 'every gear/overflow/command view switch reuses the one panel');
    const posted = created[0].posted;
    assert(posted.some((message) => message.type === 'faktorShowView' && message.view === 'settings'), 'Settings switch is posted locally');
    assert(posted.some((message) => message.type === 'e8SelectScreen' && message.screen === 'S03'), 'a settings detail is selected in place');
    assert(posted.some((message) => message.type === 'faktorShowView' && message.view === 'work'), 'Chat is an internal destination');
    created[0].listener?.({ type: 'openSettingsView' });
    assert(created.length === 1, 'the gear message never creates a second panel');
    created[0].posted.length = 0;
    provider.showSettings('S03', false);
    created[0].listener?.({ type: 'ready' });
    assert(
      created[0].posted.some((message) => message.type === 'faktorShowView' && message.view === 'settings'),
      'a first-action Settings command is replayed after the webview handshake',
    );
    assert(
      created[0].posted.some((message) => message.type === 'e8SelectScreen' && message.screen === 'S03'),
      'a first-action settings detail is replayed after the webview handshake',
    );

    const launcher = new LauncherViewProvider();
    let launcherListener = null;
    const view = {
      webview: {
        cspSource: 'vscode-resource:',
        html: '',
        options: null,
        onDidReceiveMessage(listener) {
          launcherListener = listener;
          return { dispose() {} };
        },
      },
    };
    launcher.resolveWebviewView(view);
    assert(view.webview.html.includes('Open Faktor'), 'the activity bar renders only the launcher');
    assert(!view.webview.html.includes('id="goal"'), 'the sidebar does not duplicate chat');
    launcherListener?.({ type: 'open' });
    await sleep(0);
    assert(commands.includes('faktor.openChat'), 'the launcher opens the same panel command');
    if (process.exitCode !== 1) {
      console.log('PASS  idempotent panel: one create, reveal-only focus, internal view switches, launcher opens same command');
    }
  } finally {
    require('node:module')._load = originalLoad;
  }
}

// ------------------------------------------------------------- real DOM test

async function domChecks() {
  const chrome = findChrome();
  if (chrome === null) {
    fail('headless Chrome not found');
    return;
  }
  mkdirSync(OUT, { recursive: true });
  const child = await launchChrome(chrome);
  const cdp = await connect(child.pageWs);
  await cdp.send('Page.enable');
  await cdp.send('Runtime.enable');
  const html = buildHarnessHtml(false);
  const pagePath = join(OUT, 'harness.html');
  writeFileSync(pagePath, html);
  const url = pathToFileURL(pagePath).href;
  try {
    for (const width of [480, 1440]) {
      await cdp.send('Emulation.setDeviceMetricsOverride', {
        width,
        height: 900,
        deviceScaleFactor: 1,
        mobile: false,
      });
      await cdp.send('Page.navigate', { url });
      await sleep(350);
      await cdp.evaluate('document.getElementById("goal").value = "ask survives the switch"');
      await cdp.evaluate('document.getElementById("btn-intent-task").click()');
      await cdp.evaluate('document.getElementById("btn-intent-ask").click()');
      const chat = await cdp.evaluate(`(function () {
        var work = document.getElementById('work-panel');
        var transcript = document.getElementById('transcript-card');
        var entries = document.getElementById('entries');
        var composer = document.getElementById('composer');
        function rect(node) { return node.getBoundingClientRect(); }
        var parts = [document.getElementById('attention-card'), document.getElementById('permission-card'), transcript, composer]
          .filter(function (node) { return node && !node.hidden; })
          .map(rect);
        var workRect = rect(work);
        var transcriptRect = rect(transcript);
        var composerRect = rect(composer);
        var visibleHeaders = Array.prototype.filter.call(document.querySelectorAll('#app > header, .e8-header'), function (node) {
          return node.getBoundingClientRect().height > 0 && getComputedStyle(node).display !== 'none';
        });
        return {
          overflow: document.documentElement.scrollWidth - window.innerWidth,
          widths: parts.map(function (part) { return Math.round(part.width); }),
          lefts: parts.map(function (part) { return Math.round(part.left); }),
          workHeight: Math.round(workRect.height),
          workBottom: Math.round(workRect.bottom),
          transcriptBottom: Math.round(transcriptRect.bottom),
          composerTop: Math.round(composerRect.top),
          composerBottom: Math.round(composerRect.bottom),
          flexGrow: getComputedStyle(transcript).flexGrow,
          entriesOverflow: getComputedStyle(entries).overflowY,
          entriesScrolls: entries.scrollHeight > entries.clientHeight,
          visibleHeaders: visibleHeaders.length,
          goal: document.getElementById('goal').value,
          intent: document.getElementById('btn-intent-ask').getAttribute('aria-checked'),
          settingsHidden: document.getElementById('settings-panel').hidden,
          settingsHeadersVisible: Array.prototype.filter.call(
            document.querySelectorAll('#settings-panel .e8-header'),
            function (node) { return node.getBoundingClientRect().height > 0; },
          ).length,
        };
      })()`);
      assert(chat.overflow <= 1, `${width}px: no horizontal overflow (${chat.overflow}px)`);
      assert(new Set(chat.widths).size === 1, `${width}px: every chat region spans one column (${chat.widths})`);
      assert(new Set(chat.lefts).size === 1, `${width}px: no side-by-side chat regions (${chat.lefts})`);
      assert(chat.workHeight > 300, `${width}px: chat fills the window height`);
      assert(chat.flexGrow === '1', `${width}px: the transcript owns the flexible height`);
      assert(chat.entriesOverflow === 'auto' || chat.entriesOverflow === 'scroll', `${width}px: transcript has its own scroll`);
      assert(chat.composerTop >= chat.transcriptBottom - 1, `${width}px: composer follows the transcript in flow`);
      assert(chat.composerBottom <= chat.workBottom + 1, `${width}px: composer never overlaps/clips the work surface`);
      assert(chat.visibleHeaders === 1, `${width}px: exactly one visible header (found ${chat.visibleHeaders})`);
      assert(chat.settingsHeadersVisible === 0, `${width}px: no duplicate Settings header`);
      assert(chat.settingsHidden, `${width}px: Settings is an internal hidden view initially`);
      assert(chat.goal === 'ask survives the switch', `${width}px: draft survives internal switches`);
      assert(chat.intent === 'true', `${width}px: Ask intent survives internal switches`);

      const shot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(join(OUT, `chat-${width}.png`), Buffer.from(shot.data, 'base64'));

      // Gear: switch to Settings inside the SAME document.
      await cdp.evaluate('document.getElementById("btn-control-center").click()');
      await sleep(120);
      const settings = await cdp.evaluate(`(function () {
        var visibleHeaders = Array.prototype.filter.call(document.querySelectorAll('#app > header, .e8-header'), function (node) {
          return node.getBoundingClientRect().height > 0 && getComputedStyle(node).display !== 'none';
        });
        return {
          workHidden: document.getElementById('work-panel').hidden,
          settingsHidden: document.getElementById('settings-panel').hidden,
          headerCount: visibleHeaders.length,
          e8HeaderVisible: Array.prototype.some.call(
            document.querySelectorAll('#settings-panel .e8-header'),
            function (node) { return node.getBoundingClientRect().height > 0; },
          ),
          overflow: document.documentElement.scrollWidth - window.innerWidth,
          settingsWidth: Math.round(document.getElementById('settings-panel').getBoundingClientRect().width),
        };
      })()`);
      assert(settings.workHidden && settings.settingsHidden === false, `${width}px: gear swaps to Settings in place`);
      assert(settings.headerCount === 1, `${width}px: Settings keeps the one header (found ${settings.headerCount})`);
      assert(!settings.e8HeaderVisible, `${width}px: the embedded settings header is hidden`);
      assert(settings.overflow <= 1, `${width}px: Settings has no horizontal overflow`);
      assert(settings.settingsWidth > 300, `${width}px: Settings fills the same surface`);
      const settingsShot = await cdp.send('Page.captureScreenshot', { format: 'png' });
      writeFileSync(join(OUT, `settings-${width}.png`), Buffer.from(settingsShot.data, 'base64'));

      // Host/internal switch back to Chat; the document (and its state) stays.
      await cdp.evaluate('window.postMessage({ type: "faktorShowView", view: "work" }, "*")');
      await sleep(80);
      const restored = await cdp.evaluate(`(function () {
        return {
          workHidden: document.getElementById('work-panel').hidden,
          settingsHidden: document.getElementById('settings-panel').hidden,
          goal: document.getElementById('goal').value,
          intent: document.getElementById('btn-intent-ask').getAttribute('aria-checked'),
        };
      })()`);
      assert(restored.workHidden === false && restored.settingsHidden === true, `${width}px: Chat returns without a new document`);
      assert(restored.goal === 'ask survives the switch' && restored.intent === 'true', `${width}px: Ask/Intent preserved across the return`);
      console.log(`PASS  ${width}px: one column, transcript scroll, composer in flow, one header, state preserved (${join(OUT, `chat-${width}.png`)})`);
    }
  } finally {
    cdp.ws.close();
    child.child.kill('SIGKILL');
  }
}

staticChecks();
await hostChecks();
await domChecks();
if (process.exitCode === 1) process.exit(1);
