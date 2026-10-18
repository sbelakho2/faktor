// Faktor Revision 8 webview script. Dependency-free, no eval, no remote
// loads. It never computes daemon state: it applies precomputed Edition 8
// view patches keyed by control/screen id, stages drafts locally and posts
// typed requests. Patch work is proportional to changed nodes.
(function () {
  'use strict';

  var vscode = acquireVsCodeApi();
  var app = document.getElementById('e8-app');
  if (!app) return;

  var screens = new Map();
  for (var node of document.querySelectorAll('[data-screen]')) {
    screens.set(node.getAttribute('data-screen'), node);
  }
  var links = new Map();
  for (var link of document.querySelectorAll('[data-screen-link]')) {
    links.set(link.getAttribute('data-screen-link'), link);
  }
  var destinations = new Map();
  for (var panel of document.querySelectorAll('[data-destination-panel]')) {
    destinations.set(panel.getAttribute('data-destination-panel'), panel);
  }
  var tabs = Array.prototype.slice.call(document.querySelectorAll('.e8-tab'));
  var staged = new Map();
  var unstaged = new Set();
  var selected = null;
  var applyInFlight = false;
  var noticeText = '';
  var probes = { patches: 0, controls: 0, rows: 0, fullRebuilds: 0, text: 0 };
  window.__faktorE8Probe = probes;

  function setNotice(text) {
    if (text === noticeText) return;
    noticeText = text;
    var node = document.getElementById('e8-notice');
    if (node) node.textContent = text;
  }

  function controlCard(id) {
    return document.getElementById('e8-control-' + id);
  }

  function updateControl(patch) {
    var card = controlCard(patch.id);
    if (!card) return;
    probes.controls += 1;
    card.setAttribute('data-state', patch.state);
    var badge = card.querySelector('.e8-state-badge');
    if (badge) {
      badge.textContent = patch.state;
      badge.setAttribute('data-state', patch.state);
    }
    var state = card.querySelector('[data-control-state="' + patch.id + '"]');
    if (state) {
      state.textContent = patch.stateText;
      probes.text += 1;
    }
    var help = document.getElementById('e8-desc-' + patch.id);
    if (help) {
      help.textContent = patch.valueText;
      probes.text += 1;
    }
    var activate = card.querySelector('[data-activate="' + patch.id + '"]');
    if (activate) {
      activate.disabled = patch.disabled;
      activate.setAttribute('aria-disabled', patch.disabled ? 'true' : 'false');
    }
    var inputs = card.querySelectorAll('[data-input="' + patch.id + '"]');
    for (var input of inputs) input.disabled = patch.disabled;
  }

  function rowNode(key) {
    return document.querySelector('[data-row-key="' + key + '"]');
  }

  function renderRows(screenId, signature, rows) {
    var screen = screens.get(screenId);
    if (!screen) return;
    var data = screen.querySelector('.e8-data');
    if (!data) return;
    var current = data.getAttribute('data-signature') || '';
    if (current === signature) return;
    data.setAttribute('data-signature', signature);
    probes.rows += 1;
    // Keyed update: only rows whose value/state changed are touched. The host
    // sends served values; the webview never invents a replacement.
    if (Array.isArray(rows)) {
      for (var row of rows) {
        var node = data.querySelector('[data-row-key="' + CSS.escape(row.key) + '"]');
        if (!node) {
          node = document.createElement('div');
          node.className = 'e8-row';
          node.setAttribute('data-row-key', row.key);
          var label = document.createElement('span');
          label.className = 'e8-row-label';
          node.appendChild(label);
          var value = document.createElement('span');
          value.className = 'e8-row-value';
          node.appendChild(value);
          data.appendChild(node);
        }
        var labelNode = node.querySelector('.e8-row-label');
        var valueNode = node.querySelector('.e8-row-value');
        if (labelNode && labelNode.textContent !== row.label) labelNode.textContent = row.label;
        if (valueNode) {
          if (valueNode.textContent !== row.value) valueNode.textContent = row.value;
          valueNode.setAttribute('data-state', row.state);
        }
        node.setAttribute('data-row-state', row.state);
      }
    }
  }

  function applyPatch(message) {
    probes.patches += 1;
    if (Array.isArray(message.controls)) {
      for (var patch of message.controls) updateControl(patch);
    }
    if (typeof message.screen === 'string') {
      renderRows(message.screen, message.rowsSignature || '', message.rows);
      var screen = screens.get(message.screen);
      if (screen) {
        var status = screen.querySelector('.e8-status');
        if (status && typeof message.status === 'string') {
          status.textContent = message.status;
          status.setAttribute('data-level', message.statusLevel || 'quiet');
        }
      }
    }
  }

  function display(value) {
    if (typeof value === 'string') return value.length > 80 ? value.slice(0, 77) + '\u2026' : value;
    return JSON.stringify(value);
  }

  function footerPatch() {
    var list = document.getElementById('e8-patch-list');
    var count = document.getElementById('e8-staged-count');
    if (!list || !count) return;
    var entries = Array.from(staged.values());
    count.textContent = entries.length === 0 && unstaged.size === 0
      ? 'No staged changes'
      : entries.length + ' staged \u00b7 ' + unstaged.size + ' reset';
    var signature = entries
      .map(function (entry) { return entry.controlId + '=' + JSON.stringify(entry.value); })
      .concat(Array.from(unstaged).map(function (id) { return id + '=unset'; }))
      .sort()
      .join('|');
    if (list.getAttribute('data-signature') === signature) return;
    list.setAttribute('data-signature', signature);
    list.textContent = '';
    for (var entry of entries.slice().sort(function (a, b) { return a.controlId.localeCompare(b.controlId); })) {
      var chip = document.createElement('span');
      chip.className = 'e8-chip';
      chip.textContent = entry.controlId + ' \u00b7 ' + entry.key + ' = ' + display(entry.value);
      list.appendChild(chip);
    }
    for (var id of Array.from(unstaged).sort()) {
      var chip2 = document.createElement('span');
      chip2.className = 'e8-chip e8-chip-reset';
      chip2.textContent = id + ' \u00b7 reset to inherited';
      list.appendChild(chip2);
    }
    var apply = document.getElementById('e8-apply');
    if (apply) apply.disabled = applyInFlight || (entries.length === 0 && unstaged.size === 0);
  }

  function applyDraft(message) {
    if (Array.isArray(message.set)) {
      staged = new Map();
      for (var entry of message.set) staged.set(entry.controlId, entry);
    }
    if (Array.isArray(message.unset)) unstaged = new Set(message.unset);
    if (message.error) setNotice(message.error);
    if (message.notice) setNotice(message.notice);
    footerPatch();
  }

  function selectScreen(screenId) {
    var screen = screens.get(screenId);
    if (!screen) return false;
    selected = screenId;
    var destination = screen.getAttribute('data-destination');
    for (var [key, panel] of destinations) panel.hidden = key !== destination;
    for (var [id, node] of screens) node.hidden = id !== screenId;
    for (var [id, link] of links) link.setAttribute('aria-current', id === screenId ? 'page' : 'false');
    for (var tab of tabs) {
      tab.setAttribute('aria-current', tab.getAttribute('data-destination') === destination ? 'page' : 'false');
    }
    vscode.postMessage({ type: 'e8SelectScreen', screen: screenId });
    var heading = document.getElementById('e8-screen-title-' + screenId);
    if (heading) heading.focus({ preventScroll: true });
    return true;
  }

  function actionPayload(id) {
    var field = document.querySelector('[data-action-input="' + id + '"]');
    var raw = field ? field.value.trim() : '';
    if (raw.length === 0) return {};
    try {
      var parsed = JSON.parse(raw);
      if (parsed !== null && typeof parsed === 'object' && !Array.isArray(parsed)) return parsed;
    } catch (error) {
      // Plain text is a valid payload for search/steer fields.
    }
    return { value: raw, text: raw, query: raw };
  }

  function activate(id) {
    var payload = actionPayload(id);
    vscode.postMessage({ type: 'e8Activate', controlId: id, payload: payload });
  }

  document.addEventListener('click', function (event) {
    var target = event.target;
    if (!(target instanceof Element)) return;
    var link = target.closest('[data-screen-link]');
    if (link) {
      event.preventDefault();
      selectScreen(link.getAttribute('data-screen-link'));
      return;
    }
    var open = target.closest('[data-open-screen]');
    if (open) {
      event.preventDefault();
      var targetScreen = open.getAttribute('data-open-screen');
      if (!screens.has(targetScreen)) {
        vscode.postMessage({ type: 'e8OpenScreen', screen: targetScreen });
      } else {
        selectScreen(targetScreen);
      }
      return;
    }
    var tab = target.closest('.e8-tab');
    if (tab) {
      event.preventDefault();
      var destination = tab.getAttribute('data-destination');
      var first = document.querySelector('[data-destination-panel="' + destination + '"] [data-screen]');
      if (first) selectScreen(first.getAttribute('data-screen'));
      return;
    }
    var activateButton = target.closest('[data-activate]');
    if (activateButton) {
      event.preventDefault();
      activate(activateButton.getAttribute('data-activate'));
      return;
    }
    var inspect = target.closest('[data-inspect]');
    if (inspect) {
      event.preventDefault();
      var id = inspect.getAttribute('data-inspect');
      var card = controlCard(id);
      if (card) {
        var details = card.querySelector('details');
        if (details) details.open = !details.open;
      }
      return;
    }
    var unstageButton = target.closest('[data-unstage]');
    if (unstageButton) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Unstage', controlId: unstageButton.getAttribute('data-unstage') });
      return;
    }
    if (target.closest('#e8-refresh')) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Refresh' });
      return;
    }
    if (target.closest('#e8-reset')) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Reset' });
      return;
    }
    if (target.closest('#e8-defaults')) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Defaults' });
      return;
    }
    if (target.closest('#e8-apply')) {
      event.preventDefault();
      if (applyInFlight) return;
      applyInFlight = true;
      footerPatch();
      vscode.postMessage({
        type: 'e8Apply',
        set: Array.from(staged.values()),
        unset: Array.from(unstaged),
      });
      return;
    }
  });

  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape') {
      var details = document.querySelector('details[open]');
      if (details) {
        details.open = false;
        event.preventDefault();
      }
    }
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
      var focused = document.activeElement;
      if (focused && focused.getAttribute && focused.getAttribute('data-activate')) {
        event.preventDefault();
        activate(focused.getAttribute('data-activate'));
      }
    }
  });

  // Stage on change; never on every keystroke (input latency and IME safety).
  document.addEventListener('change', function (event) {
    var target = event.target;
    if (!(target instanceof Element)) return;
    var id = target.getAttribute && target.getAttribute('data-input');
    if (!id) return;
    var raw = target.type === 'checkbox' ? target.checked : target.value;
    vscode.postMessage({ type: 'e8Stage', controlId: id, raw: raw });
  });

  window.addEventListener('message', function (event) {
    var message = event.data;
    if (message === null || typeof message !== 'object') return;
    switch (message.type) {
      case 'e8Draft':
        applyDraft(message);
        return;
      case 'e8Patch':
        applyPatch(message);
        return;
      case 'e8Notice':
        setNotice(message.message || '');
        return;
      case 'e8Error':
        setNotice(message.message || '');
        return;
      case 'e8ActionResult':
        if (applyInFlight) {
          applyInFlight = false;
          footerPatch();
        }
        setNotice(message.message || '');
        return;
      case 'e8SelectScreen':
        if (typeof message.screen === 'string' && screens.has(message.screen)) selectScreen(message.screen);
        return;
      default:
        return;
    }
  });

  if (window.__faktorE8Initial) {
    selectScreen(window.__faktorE8Initial);
  }
  footerPatch();
  vscode.postMessage({ type: 'e8Ready' });
})();
