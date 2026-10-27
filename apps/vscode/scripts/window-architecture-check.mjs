#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile,read-only-artifacts
// Change-scoped verification for the ONE Faktor surface.
//
// 1. Static architecture: ChatViewProvider is the primary activity-bar
//    webview view (`faktor.launcher`); no editor-area panel is created by
//    default; Settings/Inspect/History/Diagnostics remain internal views.
// 2. Host idempotence: focusing re-shows the same view; internal view moves
//    reuse the same document; a disposed view re-focuses the sidebar view.
// 3. Real DOM (headless Chrome): at 240/320/480 the conversation transcript
//    fills the sidebar and is visible above the fold, the empty state is at
//    most ONE compact line, no suggestion wall exists, and the composer sits
//    in flow without overlapping the transcript.
// 4. Mutation witnesses for no-suggestion-wall, sidebar-default and
//    conversation-visible, plus before/after screenshots.

import { createRequire } from 'node:module';
import {
  mkdirSync,
  readFileSync,
  writeFileSync,
} from 'node:fs';
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
const WIDTHS = [240, 320, 480];
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
  for (const [path, text] of sources) {
    assert(
      !text.includes('createWebviewPanel('),
      `${path} must not create an editor-area webview panel`,
    );
    assert(!text.includes('class Edition8Panel'), `${path} must not define a second editor panel`);
    assert(!text.includes('class Edition8ViewProvider'), `${path} must not define a sidebar settings view`);
  }
  const webview = read(join(SRC, 'webview.ts'));
  assert(webview.includes("viewType = 'faktor.launcher'"), 'the primary surface is faktor.launcher');
  assert(webview.includes('implements vscode.WebviewViewProvider'), 'the chat is a webview view');
  assert(webview.includes('resolveWebviewView('), 'the view resolves into the activity bar');
  assert(webview.includes('view.show(true)'), 'focus reveals the existing sidebar view');
  assert(webview.includes('`${ChatViewProvider.viewType}.focus`'), 'an unresolved view focuses the sidebar view id');
  assert(webview.includes('data-surface="sidebar"'), 'the document marks the sidebar surface');
  assert(webview.includes('id="settings-panel"'), 'Settings is an internal view of the same document');
  assert(webview.includes('renderEdition8AppMarkup'), 'Settings markup renders inside the one document');
  assert(webview.includes('id="menu-settings"'), 'the overflow exposes Settings');
  assert(webview.includes('faktorShowView'), 'the host can switch internal views');
  assert(!webview.includes('welcome-chip'), 'no suggestion chips remain in the markup');
  assert(!webview.includes('welcome-prompts'), 'no suggestion list remains in the markup');
  assert(!webview.includes('welcome-lead'), 'no welcome wall copy remains in the markup');
  assert(webview.includes('class="composer-toolbar"'), 'the composer is one compact toolbar row');
  assert(!webview.includes('composer-chips'), 'the composer chip row is gone');
  assert(!webview.includes('attachment-hint'), 'the attach hint is gone');
  assert(!webview.includes('Ask Faktor…</label>'), 'the big composer label is gone');

  const chat = read(join(SRC, '..', 'media', 'chat.js'));
  assert(chat.includes("settings: 'settings-panel'"), 'chat.js routes Settings locally');
  assert(chat.includes("byId('menu-settings')"), 'the overflow Settings item switches internally');
  assert(chat.includes("byId('btn-control-center')"), 'the gear switches internally');
  assert(chat.includes("message.type === 'faktorShowView'"), 'the host view switch is handled locally');
  assert(!chat.includes('welcome-chip'), 'chat.js ships no suggestion-chip behavior');
  assert(!chat.includes("type: 'openControlCenter'"), 'the gear never asks the host for another panel');

  const extension = read(join(SRC, 'extension.ts'));
  assert(
    extension.includes('registerWebviewViewProvider(ChatViewProvider.viewType, chatProvider'),
    'the extension registers the chat as the primary activity-bar view',
  );
  assert(!extension.includes('LauncherViewProvider'), 'the obsolete launcher card is gone');
  assert(!extension.includes('Edition8Panel.createOrShow'), 'no command creates a settings panel');
  assert(extension.includes('showEdition8Screen'), 'Edition 8 commands target internal views');

  const pkg = JSON.parse(read(join(SRC, '..', 'package.json')));
  const viewContainers = Object.keys(pkg.contributes.viewsContainers ?? {});
  assert(viewContainers.includes('activitybar'), 'the primary container is the activity bar');
  const container = pkg.contributes.viewsContainers.activitybar.find((entry) => entry.id === 'faktor');
  assert(container !== undefined, 'the Faktor activity-bar container is contributed');
  const views = Object.values(pkg.contributes.views).flat();
  assert(views.length === 1, `one activity-bar view, found ${views.length}`);
  assert(views[0].id === 'faktor.launcher', 'the activity-bar entry hosts the chat view');
  assert(views[0].type === 'webview', 'the activity-bar entry is a webview view');
  assert(
    !views.some((entry) => entry.id === 'faktor.chat' || entry.id === 'faktor.edition8'),
    'no duplicate chat/settings sidebar views remain',
  );
  assert(
    pkg.activationEvents.includes('onView:faktor.launcher'),
    'activation follows the primary sidebar view',
  );
  if (process.exitCode !== 1) {
    console.log('PASS  sidebar-default: one activity-bar webview view, no editor panel, internal Settings/Inspect/History/Diagnostics');
  }
}

// ----------------------------------------------------- host idempotence test

async function hostChecks() {
  const panels = [];
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
  function fakeView(label) {
    const view = {
      label,
      shown: [],
      posted: [],
      disposed: false,
      webview: {
        cspSource: 'vscode-resource:',
        html: '',
        options: null,
        asWebviewUri: (uri) => uri,
        postMessage: async (message) => {
          view.posted.push(message);
          return true;
        },
        onDidReceiveMessage: (listener) => {
          view.listener = listener;
          return { dispose() {} };
        },
      },
      show(preserveFocus) {
        view.shown.push({ preserveFocus });
      },
      onDidDispose: (listener) => {
        view.disposeListener = listener;
        return { dispose() {} };
      },
    };
    return view;
  }
  const vscode = {
    Uri: FakeUri,
    ViewColumn: { Active: -1, One: 1, Two: 2 },
    window: {
      createWebviewPanel() {
        panels.push(true);
        throw new Error('the chat must never create an editor-area panel');
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
    const { ChatViewProvider } = require('../out/webview.js');
    const host = { handle: () => {}, onViewResolved: () => {} };
    const provider = new ChatViewProvider(
      { extensionUri: new FakeUri('/extension'), subscriptions: [] },
      host,
    );
    provider.focus();
    assert(panels.length === 0, 'the default surface never creates an editor panel');
    assert(
      commands.includes('faktor.launcher.focus'),
      'focus before resolution reveals the activity-bar view',
    );

    const view = fakeView('first');
    provider.resolveWebviewView(view);
    assert(view.webview.html.includes('data-surface="sidebar"'), 'the resolved view renders the sidebar surface');
    assert(view.webview.html.includes('id="goal"'), 'the resolved view includes the composer');
    assert(!view.webview.html.includes('welcome-prompts'), 'the resolved view has no suggestion wall');
    assert(view.show.length >= 0, 'the view exposes show()');

    view.posted.length = 0;
    provider.focus();
    provider.focus();
    assert(view.shown.length === 2, `focus re-shows the same view (shown ${view.shown.length})`);
    assert(panels.length === 0, 'focus never creates a panel');
    assert(
      view.posted.some((message) => message.type === 'faktorShowView' && message.view === 'work'),
      'Chat is the default internal destination',
    );

    view.posted.length = 0;
    provider.showSettings('S03', false);
    provider.showView('inspect');
    provider.showView('history');
    provider.showView('diagnostics');
    provider.showView('work');
    provider.showSettings('S29', true);
    provider.showEdition8Screen('S41');
    provider.showEdition8Screen('S26');
    provider.showEdition8Screen('S03');
    assert(panels.length === 0, 'every gear/overflow/command view switch reuses the one view');
    assert(
      view.posted.some((message) => message.type === 'faktorShowView' && message.view === 'settings'),
      'Settings switch is posted locally',
    );
    assert(
      view.posted.some((message) => message.type === 'e8SelectScreen' && message.screen === 'S03'),
      'a settings detail is selected in place',
    );
    assert(
      view.posted.some((message) => message.type === 'faktorShowView' && message.view === 'work'),
      'Chat is an internal destination',
    );

    view.listener?.({ type: 'openSettingsView' });
    assert(panels.length === 0, 'the gear message never creates a panel');
    view.posted.length = 0;
    provider.showSettings('S03', false);
    view.listener?.({ type: 'ready' });
    assert(
      view.posted.some((message) => message.type === 'faktorShowView' && message.view === 'settings'),
      'a first-action Settings command is replayed after the webview handshake',
    );
    assert(
      view.posted.some((message) => message.type === 'e8SelectScreen' && message.screen === 'S03'),
      'a first-action settings detail is replayed after the webview handshake',
    );

    // A hidden view may be disposed; the next command must focus the same
    // sidebar view id, never fall back to an editor tab.
    view.disposeListener?.();
    commands.length = 0;
    provider.focus();
    assert(panels.length === 0, 'a disposed view still never creates an editor panel');
    assert(
      commands.includes('faktor.launcher.focus'),
      'a disposed view re-focuses the primary activity-bar view',
    );

    const second = fakeView('second');
    provider.resolveWebviewView(second);
    assert(second.webview.html.includes('data-surface="sidebar"'), 'a re-resolved view keeps the sidebar surface');
    assert(panels.length === 0, 're-resolution never creates an editor panel');
    if (process.exitCode !== 1) {
      console.log('PASS  idempotent sidebar view: re-show only, internal switches, no editor panel');
    }
  } finally {
    require('node:module')._load = originalLoad;
  }
}

// ------------------------------------------------------------- real DOM test

const PROBE = `(function () {
  var rect = function (node) {
    var r = node.getBoundingClientRect();
    return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, width: r.width, height: r.height };
  };
  var visible = function (node) {
    return !!node && !node.hidden && node.getClientRects().length > 0 && getComputedStyle(node).display !== 'none';
  };
  var work = document.getElementById('work-panel');
  var header = document.querySelector('#app > header');
  var transcript = document.getElementById('transcript-card');
  var entries = document.getElementById('entries');
  var composer = document.getElementById('composer');
  var goal = document.getElementById('goal');
  var welcome = document.getElementById('welcome');
  var entryNodes = entries ? Array.prototype.slice.call(entries.children) : [];
  var entriesRect = entries ? rect(entries) : null;
  var topmostVisible = null;
  var bestVisibleOverlap = 0;
  if (entries) {
    for (var i = 0; i < entryNodes.length; i += 1) {
      var entryRect = rect(entryNodes[i]);
      var overlap =
        Math.min(entryRect.bottom, entriesRect.bottom) -
        Math.max(entryRect.top, entriesRect.top);
      if (overlap > 0 && topmostVisible === null) {
        topmostVisible = entryRect;
      }
      if (overlap > bestVisibleOverlap) {
        bestVisibleOverlap = overlap;
      }
    }
  }
  var transcriptRect = transcript ? rect(transcript) : null;
  var composerRect = composer ? rect(composer) : null;
  var workRect = work ? rect(work) : null;
  var welcomeRect = welcome && visible(welcome) ? rect(welcome) : null;
  var welcomeButtons = welcome ? welcome.querySelectorAll('button') : [];
  return {
    surface: document.body.getAttribute('data-surface'),
    overflow: document.documentElement.scrollWidth - window.innerWidth,
    innerHeight: window.innerHeight,
    header: header && visible(header) ? rect(header) : null,
    work: workRect,
    transcript: transcriptRect,
    entries: entriesRect,
    composer: composerRect,
    composerPosition: composer ? getComputedStyle(composer).position : null,
    goal: goal ? rect(goal) : null,
    entryCount: entryNodes.length,
    topmostVisibleEntry: topmostVisible,
    bestVisibleOverlap: bestVisibleOverlap,
    welcome: welcome
      ? {
          visible: visible(welcome),
          hidden: welcome.hidden,
          height: welcomeRect ? welcomeRect.height : 0,
          text: String(welcome.textContent || '').trim(),
          buttons: welcomeButtons.length,
        }
      : null,
    suggestionWall: document.querySelectorAll(
      '.welcome-chip, .welcome-prompts, .welcome-lead, .welcome-mark, .suggestion, [data-suggestion]',
    ).length,
    chatTabs: work
      ? work.querySelectorAll('[role="tab"], .nav-tab, #nav-switcher, [id^="nav-"]').length
      : 0,
  };
})()`;

function conversationVisible(probe) {
  if (
    probe.transcript === null ||
    probe.entries === null ||
    probe.transcript.height <= 40 ||
    probe.entries.height <= 40 ||
    probe.entryCount === 0 ||
    probe.topmostVisibleEntry === null
  ) {
    return false;
  }
  // The transcript follows the newest turn (initial load pins to the bottom),
  // so the top message may be clipped at a 240-320px width. The surface is
  // conversation-first when a real turn occupies a readable slice of the
  // visible transcript above the composer.
  return probe.bestVisibleOverlap >= 24;
}

function assertLayout(probe, context) {
  assert(probe.surface === 'sidebar', `${context}: surface is ${probe.surface}, expected sidebar`);
  assert(probe.overflow <= 1, `${context}: no horizontal overflow (${probe.overflow}px)`);
  assert(probe.header !== null, `${context}: the product header is visible`);
  assert(probe.header.height <= 64, `${context}: the header stays compact (${probe.header.height}px)`);
  assert(probe.chatTabs === 0, `${context}: the chat body has no tabs (${probe.chatTabs})`);
  assert(probe.suggestionWall === 0, `${context}: no suggestion wall is present (${probe.suggestionWall})`);
  assert(probe.transcript !== null && probe.transcript.height > 80, `${context}: transcript fills the sidebar`);
  assert(probe.entries !== null && probe.entries.height > 40, `${context}: transcript content area is visible`);
  assert(probe.transcript.top >= probe.header.bottom - 0.5, `${context}: transcript overlaps the header`);
  assert(probe.composerPosition !== 'fixed' && probe.composerPosition !== 'absolute' && probe.composerPosition !== 'sticky', `${context}: composer is in flow (${probe.composerPosition})`);
  assert(probe.composer.top >= probe.transcript.bottom - 1, `${context}: composer overlaps the transcript`);
  assert(probe.composer.bottom <= probe.innerHeight + 1, `${context}: composer leaves the viewport`);
  assert(
    probe.composer.height <= 140,
    `${context}: composer stays compact (${probe.composer.height}px > 140px)`,
  );
  assert(
    probe.goal !== null && probe.goal.height >= 34 && probe.goal.height <= 122,
    `${context}: the one input is 36-120px (${probe.goal ? probe.goal.height : 'missing'}px)`,
  );
  assert(conversationVisible(probe), `${context}: conversation content is not visible above the fold`);
}

function screenshot(cdp, path) {
  return cdp.send('Page.captureScreenshot', { format: 'png' }).then((shot) => {
    writeFileSync(path, Buffer.from(shot.data, 'base64'));
  });
}

async function measure(cdp, url, width, height = 900) {
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
    if (ready) break;
    await sleep(50);
  }
  await cdp.evaluate('new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))');
  return cdp.evaluate(PROBE);
}

function writePage(html, prefix) {
  const dir = require('node:fs').mkdtempSync(join(require('node:os').tmpdir(), prefix));
  const path = join(dir, 'index.html');
  writeFileSync(path, html);
  return pathToFileURL(path).href;
}

function injectSuggestionWall(html) {
  const point = html.indexOf('<div id="entries"');
  if (point < 0) throw new Error('could not find the transcript in the harness HTML');
  return (
    html.slice(0, point) +
    '<div class="welcome-prompts"><button class="welcome-chip">Find and fix the failing tests</button></div>' +
    html.slice(point)
  );
}

function injectConversationHidingWall(html) {
  const point = html.indexOf('<section id="transcript-card"');
  if (point < 0) throw new Error('could not find the transcript card in the harness HTML');
  return (
    html.slice(0, point) +
    '<div class="mutated-suggestion-wall" style="flex:none;height:900px">suggestion wall</div>' +
    html.slice(point)
  );
}

function mutateEditorSurface(html) {
  return html.replace('data-surface="sidebar"', 'data-surface="editor"');
}

function injectBloatedComposer(html) {
  const point = html.indexOf('<form id="composer"');
  if (point < 0) throw new Error('could not find the composer in the harness HTML');
  const open = html.indexOf('>', point) + 1;
  return (
    html.slice(0, open) +
    '<div class="mutated-composer-wall" style="flex:none;height:220px">composer wall</div>' +
    html.slice(open)
  );
}

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
  const populated = buildHarnessHtml(false);
  const empty = buildHarnessHtml(true);
  const populatedUrl = writePage(populated, 'faktor-window-populated-');
  const emptyUrl = writePage(empty, 'faktor-window-empty-');
  try {
    for (const width of WIDTHS) {
      const probe = await measure(cdp, populatedUrl, width);
      assertLayout(probe, `sidebar@${width}px`);
      assert(probe.welcome !== null && probe.welcome.hidden, `${width}px: the empty line disappears after the first message`);
      assert(conversationVisible(probe), `${width}px: conversation-visible`);
      await screenshot(cdp, join(OUT, `sidebar-${width}.png`));
      console.log(`PASS  ${width}px: conversation visible above the fold (${join(OUT, `sidebar-${width}.png`)})`);

      const emptyProbe = await measure(cdp, emptyUrl, width);
      assert(emptyProbe.surface === 'sidebar', `${width}px empty: sidebar surface`);
      assert(emptyProbe.overflow <= 1, `${width}px empty: no horizontal overflow`);
      assert(emptyProbe.suggestionWall === 0, `${width}px empty: no suggestion wall`);
      assert(
        emptyProbe.welcome !== null && emptyProbe.welcome.visible,
        `${width}px empty: the compact empty line is visible`,
      );
      assert(
        emptyProbe.welcome.height <= 28,
        `${width}px empty: the empty state is at most one compact line (${emptyProbe.welcome.height}px)`,
      );
      assert(
        emptyProbe.welcome.buttons === 0,
        `${width}px empty: the empty state has no buttons or prompt chips`,
      );
      assert(
        emptyProbe.transcript !== null && emptyProbe.transcript.height > 80,
        `${width}px empty: the transcript still fills the sidebar`,
      );
      assert(
        emptyProbe.composer !== null && emptyProbe.composer.bottom <= emptyProbe.innerHeight + 1,
        `${width}px empty: the composer stays in the viewport`,
      );
      assert(
        emptyProbe.composer !== null && emptyProbe.composer.height <= 140,
        `${width}px empty: the composer stays compact (${emptyProbe.composer ? emptyProbe.composer.height : 'missing'}px)`,
      );
      await screenshot(cdp, join(OUT, `sidebar-empty-${width}.png`));
      console.log(`PASS  ${width}px: compact empty line, no suggestion wall (${join(OUT, `sidebar-empty-${width}.png`)})`);
    }

    // Every Settings/Inspect/History/Diagnostics destination is an internal
    // view of the SAME sidebar document: no editor tab, no split and no new
    // webview. Chat stays the default destination and its draft survives.
    const internalViews = {
      inspect: 'inspect-panel',
      history: 'history-panel',
      diagnostics: 'diagnostics',
      settings: 'settings-panel',
    };
    await measure(cdp, populatedUrl, 320);
    await cdp.evaluate('document.getElementById("goal").value = "draft survives"');
    for (const [key, panelId] of Object.entries(internalViews)) {
      await cdp.evaluate(`document.getElementById('menu-${key}').click()`);
      await sleep(80);
      const state = await cdp.evaluate(`(function () {
        var panels = ['work-panel', 'inspect-panel', 'history-panel', 'diagnostics', 'settings-panel'];
        var visible = panels.filter(function (id) { var node = document.getElementById(id); return node && !node.hidden; });
        return {
          visible: visible,
          targetHidden: document.getElementById('${panelId}').hidden,
          workHidden: document.getElementById('work-panel').hidden,
          overflow: document.documentElement.scrollWidth - window.innerWidth,
          headers: Array.prototype.filter.call(document.querySelectorAll('#app > header, .e8-header'), function (node) {
            return node.getBoundingClientRect().height > 0 && getComputedStyle(node).display !== 'none';
          }).length,
        };
      })()`);
      assert(state.targetHidden === false, `internal ${key}: destination is visible`);
      assert(state.workHidden === true, `internal ${key}: chat is not the active destination`);
      assert(state.visible.length === 1, `internal ${key}: exactly one destination is visible (${state.visible.join(',')})`);
      assert(state.overflow <= 1, `internal ${key}: no horizontal overflow`);
      assert(state.headers === 1, `internal ${key}: exactly one header (found ${state.headers})`);
      await screenshot(cdp, join(OUT, `sidebar-${key}-320.png`));
    }
    await cdp.evaluate('document.getElementById("menu-chat").click()');
    await sleep(80);
    const restored = await cdp.evaluate(`(function () {
      return {
        workHidden: document.getElementById('work-panel').hidden,
        otherHidden: ['inspect-panel', 'history-panel', 'diagnostics', 'settings-panel']
          .every(function (id) { return document.getElementById(id).hidden; }),
        draft: document.getElementById('goal').value,
      };
    })()`);
    assert(restored.workHidden === false && restored.otherHidden === true, 'internal views return to Chat in place');
    assert(restored.draft === 'draft survives', 'the Chat draft survives internal view switches');
    console.log('PASS  Settings/Inspect/History/Diagnostics are internal sidebar views (no editor tab)');

    // Mutation witness 1: a suggestion wall must be observed by the same probe.
    const wallUrl = writePage(injectSuggestionWall(populated), 'faktor-window-wall-');
    const wallProbe = await measure(cdp, wallUrl, 320);
    assert(wallProbe.suggestionWall > 0, 'no-suggestion-wall witness did not detect an injected wall');
    console.log(`PASS  no-suggestion-wall witness observed ${wallProbe.suggestionWall} suggestion element(s)`);

    // Mutation witness 2: an editor-tab surface marker must flip sidebar-default.
    const editorUrl = writePage(mutateEditorSurface(populated), 'faktor-window-editor-');
    const editorProbe = await measure(cdp, editorUrl, 320);
    assert(editorProbe.surface !== 'sidebar', 'sidebar-default witness did not detect an editor surface');
    console.log(`PASS  sidebar-default witness observed surface ${editorProbe.surface}`);

    // Mutation witness 3: a wall that pushes the conversation down must flip
    // conversation-visible.
    const hideUrl = writePage(injectConversationHidingWall(populated), 'faktor-window-hidden-');
    const hideProbe = await measure(cdp, hideUrl, 320);
    assert(
      !conversationVisible(hideProbe),
      'conversation-visible witness still reported the hidden conversation as visible',
    );
    console.log('PASS  conversation-visible witness detected the conversation pushed below the fold');

    // Mutation witness 4: a bloated composer must exceed the compact bound.
    const bloatedUrl = writePage(injectBloatedComposer(populated), 'faktor-window-bloated-');
    const bloatedProbe = await measure(cdp, bloatedUrl, 320);
    assert(
      bloatedProbe.composer !== null && bloatedProbe.composer.height > 140,
      `compact-composer witness did not detect the bloated composer (${bloatedProbe.composer ? bloatedProbe.composer.height : 'missing'}px)`,
    );
    console.log(
      `PASS  compact-composer witness observed a ${Math.round(bloatedProbe.composer.height)}px composer`,
    );
  } finally {
    cdp.ws.close();
    child.child.kill('SIGKILL');
  }
}

staticChecks();
await hostChecks();
await domChecks();
if (process.exitCode === 1) process.exit(1);
