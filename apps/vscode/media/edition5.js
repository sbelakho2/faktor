// Faktor Control Center webview script. Hand-written, dependency-free, no
// eval and no remote loads. It never computes daemon state: it applies the
// precomputed Edition 5 view patches from the host, stages drafts locally and
// posts typed requests. Per-patch DOM work is keyed by control/row id and the
// probe counts exactly the nodes it touches.
(function () {
  'use strict';

  var vscode = acquireVsCodeApi();
  var app = document.getElementById('e5-app');
  if (!app) {
    return;
  }

  var screens = new Map();
  var screensById = new Map();
  for (var screenNode of document.querySelectorAll('[data-screen]')) {
    screensById.set(screenNode.getAttribute('data-screen'), screenNode);
  }
  var links = new Map();
  for (var link of document.querySelectorAll('[data-screen-link]')) {
    links.set(link.getAttribute('data-screen-link'), link);
  }
  var routerTabs = Array.prototype.slice.call(
    document.querySelectorAll('#e5-router [data-destination]'),
  );
  var destinations = new Map();
  for (var panel of document.querySelectorAll('[data-destination-panel]')) {
    destinations.set(panel.getAttribute('data-destination-panel'), panel);
  }

  var staged = new Map();
  var unstaged = new Set();
  var selected = null;
  var selectedDestination = null;
  var lastSelection = null;
  var inspected = null;
  var applyInFlight = false;
  var notices = '';
  var probes = { patches: 0, controls: 0, rows: 0, options: 0, text: 0, fullRebuilds: 0 };
  if (!window.__faktorE5Probe) {
    window.__faktorE5Probe = probes;
  }

  function controlCard(id) {
    return document.getElementById('e5-control-' + id);
  }

  function setNotice(text) {
    if (text === notices) {
      return;
    }
    notices = text;
    var node = document.getElementById('e5-notice');
    if (node) {
      node.textContent = text;
    }
  }

  function setApplyState(text, level) {
    var node = document.getElementById('e5-apply-state');
    if (!node) {
      return;
    }
    node.textContent = text;
    node.setAttribute('data-level', level || 'info');
  }

  function footerPatch() {
    var list = document.getElementById('e5-patch-list');
    var count = document.getElementById('e5-staged-count');
    if (!list || !count) {
      return;
    }
    var entries = Array.from(staged.values());
    if (entries.length === 0 && unstaged.size === 0) {
      count.textContent = 'No staged changes';
    } else {
      count.textContent = entries.length + ' staged · ' + unstaged.size + ' reset';
    }
    var signature = entries
      .map(function (entry) {
        return entry.controlId + '=' + JSON.stringify(entry.value);
      })
      .concat(Array.from(unstaged).map(function (id) { return id + '=unset'; }))
      .sort()
      .join('|');
    if (list.getAttribute('data-signature') === signature) {
      return;
    }
    list.setAttribute('data-signature', signature);
    list.textContent = '';
    var ordered = entries.slice().sort(function (a, b) {
      return a.controlId.localeCompare(b.controlId);
    });
    for (var entry of ordered) {
      var chip = document.createElement('span');
      chip.className = 'e5-chip';
      chip.textContent = entry.controlId + ' · ' + entry.key + ' = ' + display(entry.value);
      list.appendChild(chip);
    }
    for (var id of Array.from(unstaged).sort()) {
      var chip2 = document.createElement('span');
      chip2.className = 'e5-chip e5-chip-reset';
      chip2.textContent = id + ' · reset to inherited';
      list.appendChild(chip2);
    }
    var apply = document.getElementById('e5-apply');
    if (apply) {
      apply.disabled = applyInFlight || (entries.length === 0 && unstaged.size === 0) || app.getAttribute('data-stale') === 'true';
    }
  }

  function display(value) {
    if (typeof value === 'string') {
      return value.length > 80 ? value.slice(0, 77) + '…' : value;
    }
    return JSON.stringify(value);
  }

  function markStaged(id, entry, error) {
    var card = controlCard(id);
    if (card) {
      card.setAttribute('data-staged', error ? 'error' : 'true');
      var state = card.querySelector('[data-control-state="' + id + '"]');
      if (state) {
        state.textContent = error ? error : 'Staged locally; Apply sends one atomic patch against the current revision.';
        state.setAttribute('data-level', error ? 'error' : 'info');
      }
    }
    if (error) {
      setApplyState(error, 'error');
      return;
    }
    if (entry) {
      staged.set(id, entry);
      unstaged.delete(id);
    }
    footerPatch();
  }

  function applyDraft(message) {
    if (Array.isArray(message.set)) {
      staged = new Map();
      for (var entry of message.set) {
        staged.set(entry.controlId, entry);
      }
    }
    if (Array.isArray(message.unset)) {
      unstaged = new Set(message.unset);
    }
    if (message.error) {
      setApplyState(message.error, 'error');
    }
    footerPatch();
  }

  function currentInput(id) {
    var input = document.querySelector('[data-input="' + id + '"]');
    if (!input) {
      return undefined;
    }
    if (input.type === 'checkbox') {
      return input.checked;
    }
    return input.value;
  }

  function stageFromInput(id) {
    var input = document.querySelector('[data-input="' + id + '"]');
    if (!input) {
      return;
    }
    if (input.tagName === 'OUTPUT' || input.getAttribute('data-kind') === 'action') {
      return;
    }
    vscode.postMessage({ type: 'e5Stage', controlId: id, raw: currentInput(id) });
  }

  function activate(id, extra) {
    var card = controlCard(id);
    if (!card) {
      return;
    }
    var kind = card.getAttribute('data-kind');
    var command = card.getAttribute('data-command');
    if (kind === 'action' || command === 'host.action' || command === 'inspect' || command !== 'config.set') {
      vscode.postMessage({
        type: 'e5Activate',
        controlId: id,
        payload: Object.assign({}, extra || {}, { value: currentInput(id) }),
      });
      return;
    }
    if (kind === 'readonly') {
      inspect(id);
      return;
    }
    var input = document.querySelector('[data-input="' + id + '"]');
    var raw = currentInput(id);
    if (input && (raw === '' || raw === undefined || raw === null)) {
      vscode.postMessage({
        type: 'e5Stage',
        controlId: id,
        raw: null,
        useSample: true,
        value: null,
      });
      return;
    }
    vscode.postMessage({ type: 'e5Stage', controlId: id, raw: raw, activate: true, value: raw });
  }

  function inspect(id) {
    if (inspected !== null) {
      var previous = controlCard(inspected);
      if (previous) {
        previous.removeAttribute('data-inspected');
      }
    }
    inspected = id;
    var card = controlCard(id);
    if (card) {
      card.setAttribute('data-inspected', 'true');
      card.focus({ preventScroll: false });
    }
    vscode.postMessage({ type: 'e5Inspect', controlId: id });
    setNotice('Inspecting ' + id + ' · the contract, served state and source are shown in the card.');
  }

  function collectToolbar() {
    var payload = {};
    var mapping = {
      'provider-label': 'label',
      'provider-kind': 'kind',
      'provider-url': 'base_url',
      'provider-credential': 'credential_reference',
      'model-search': 'query',
      'context-uri': 'uri',
      'policy-tool': 'tool',
      'policy-target': 'target',
      'finish-level': 'level',
      'settings-query': 'query',
      'history-query': 'query',
      'transcript-query': 'query',
      'mcp-name': 'name',
      'mcp-command': 'command',
      'mcp-args': 'args',
      'steer-text': 'text',
    };
    for (var node of document.querySelectorAll('[data-toolbar]')) {
      var key = node.getAttribute('data-toolbar');
      var value = node.value;
      if (key === 'mcp-args') {
        payload[mapping[key]] = value
          .split(/\r?\n/)
          .map(function (entry) { return entry.trim(); })
          .filter(function (entry) { return entry.length > 0; });
      } else {
        payload[mapping[key] || key] = value;
      }
    }
    return payload;
  }

  function dispatchAction(button) {
    var action = button.getAttribute('data-action');
    var extra = {};
    if (button.getAttribute('data-decision')) {
      extra.decision = button.getAttribute('data-decision');
    }
    if (button.getAttribute('data-lifecycle')) {
      extra.action = button.getAttribute('data-lifecycle');
    }
    if (button.getAttribute('data-request-id')) {
      extra.request_id = button.getAttribute('data-request-id');
    }
    vscode.postMessage({
      type: 'e5Action',
      command: action,
      payload: Object.assign(collectToolbar(), extra),
    });
    setNotice('Requested ' + action + ' from the daemon; the state updates only after its acknowledgement.');
  }

  function selectScreen(screenId, focus) {
    var screen = screensById.get(screenId);
    if (!screen) {
      return;
    }
    var destination = screen.getAttribute('data-destination');
    for (var [id, node] of screensById) {
      node.hidden = id !== screenId;
    }
    for (var [id2, linkNode] of links) {
      linkNode.setAttribute('aria-current', id2 === screenId ? 'page' : 'false');
      linkNode.tabIndex = id2 === screenId ? 0 : -1;
    }
    if (destination !== selectedDestination) {
      selectDestination(destination, false);
    }
    selected = screenId;
    lastSelection = screenId;
    if (typeof window.scrollTo === 'function') {
      window.scrollTo(0, 0);
    }
    var footerScreen = document.querySelector('[data-field="screen"]');
    if (footerScreen) {
      footerScreen.textContent = screenId + ' · ' + (screen.querySelector('h2') || {}).textContent;
    }
    var link = links.get(screenId);
    if (focus && link) {
      link.focus();
    }
    if (window.location.hash !== '#' + screenId) {
      history.replaceState(null, '', '#' + screenId);
    }
  }

  function selectDestination(destination, focus) {
    selectedDestination = destination;
    for (var tab of routerTabs) {
      var active = tab.getAttribute('data-destination') === destination;
      tab.setAttribute('aria-selected', active ? 'true' : 'false');
      tab.tabIndex = active ? 0 : -1;
      if (active && focus) {
        tab.focus();
      }
    }
    for (var [key, panel] of destinations) {
      panel.hidden = key !== destination;
    }
    var first = document.querySelector('[data-destination-panel="' + destination + '"] [data-screen]:not([hidden])');
    if (!first) {
      var newScreen = document.querySelector('[data-destination-panel="' + destination + '"] [data-screen]');
      if (newScreen) {
        selectScreen(newScreen.getAttribute('data-screen'), false);
      }
    }
  }

  function stepTab(list, currentIndex, delta) {
    var next = (currentIndex + delta + list.length) % list.length;
    var target = list[next];
    var destination = target.getAttribute('data-destination');
    selectDestination(destination, false);
    var screen = document.querySelector('[data-destination-panel="' + destination + '"] [data-screen]:not([hidden])');
    if (screen) {
      selectScreen(screen.getAttribute('data-screen'), false);
    }
    target.focus();
  }

  for (var tab of routerTabs) {
    tab.addEventListener('click', function (event) {
      selectDestination(event.currentTarget.getAttribute('data-destination'), false);
    });
    tab.addEventListener('keydown', function (event) {
      var index = routerTabs.indexOf(event.currentTarget);
      if (event.key === 'ArrowRight') {
        event.preventDefault();
        stepTab(routerTabs, index, 1);
      } else if (event.key === 'ArrowLeft') {
        event.preventDefault();
        stepTab(routerTabs, index, -1);
      } else if (event.key === 'Home') {
        event.preventDefault();
        stepTab(routerTabs, 0, 0);
      } else if (event.key === 'End') {
        event.preventDefault();
        stepTab(routerTabs, routerTabs.length - 1, 0);
      }
    });
  }

  document.addEventListener('click', function (event) {
    var target = event.target;
    if (!target || !target.closest) {
      return;
    }
    var screenLink = target.closest('[data-screen-link]');
    if (screenLink) {
      selectScreen(screenLink.getAttribute('data-screen-link'), true);
      return;
    }
    var activateButton = target.closest('[data-activate]');
    if (activateButton && !activateButton.disabled) {
      var bound = activateButton.getAttribute('data-request-id');
      activate(activateButton.getAttribute('data-activate'), bound ? { request_id: bound } : {});
      return;
    }
    var inspectButton = target.closest('[data-inspect]');
    if (inspectButton) {
      inspect(inspectButton.getAttribute('data-inspect'));
      return;
    }
    var unstageButton = target.closest('[data-unstage]');
    if (unstageButton && !unstageButton.disabled) {
      vscode.postMessage({ type: 'e5Unstage', controlId: unstageButton.getAttribute('data-unstage') });
      return;
    }
    var actionButton = target.closest('[data-action]');
    if (actionButton && !actionButton.disabled) {
      dispatchAction(actionButton);
      return;
    }
    if (target.closest('#e5-reset')) {
      vscode.postMessage({ type: 'e5Reset' });
      return;
    }
    if (target.closest('#e5-defaults')) {
      vscode.postMessage({ type: 'e5Defaults' });
      return;
    }
    if (target.closest('#e5-apply') && !target.closest('#e5-apply').disabled) {
      if (app.getAttribute('data-stale') === 'true') {
        setApplyState('Apply is disabled while the observations are stale. Reconcile the session first.', 'error');
        return;
      }
      applyInFlight = true;
      footerPatch();
      vscode.postMessage({
        type: 'e5Apply',
        set: Array.from(staged.values()),
        unset: Array.from(unstaged),
      });
    }
  });

  document.addEventListener('change', function (event) {
    var input = event.target;
    if (!input || !input.getAttribute) {
      return;
    }
    var id = input.getAttribute('data-input');
    if (id) {
      stageFromInput(id);
    }
  });

  document.addEventListener('input', function (event) {
    var input = event.target;
    if (input && input.getAttribute && input.getAttribute('data-input')) {
      // Staging happens on change so typing is never turned into a patch per
      // keystroke; the control's local state stays untouched until then.
      var state = document.querySelector('[data-control-state="' + input.getAttribute('data-input') + '"]');
      if (state && state.getAttribute('data-level') !== 'error') {
        state.textContent = 'Edited locally; staged on change.';
      }
    }
  });

  document.addEventListener('keydown', function (event) {
    if (event.key === 'Escape') {
      if (lastSelection !== null) {
        selectScreen(lastSelection, false);
      }
      return;
    }
    var link = event.target && event.target.closest ? event.target.closest('[data-screen-link]') : null;
    if (!link) {
      return;
    }
    var all = Array.prototype.slice.call(
      document.querySelectorAll('[data-destination-panel="' + selectedDestination + '"] [data-screen-link]'),
    );
    var index = all.indexOf(link);
    var next = null;
    if (event.key === 'ArrowDown') {
      next = all[(index + 1) % all.length];
    } else if (event.key === 'ArrowUp') {
      next = all[(index - 1 + all.length) % all.length];
    } else if (event.key === 'Home') {
      next = all[0];
    } else if (event.key === 'End') {
      next = all[all.length - 1];
    }
    if (next) {
      event.preventDefault();
      selectScreen(next.getAttribute('data-screen-link'), true);
    }
  });

  function updateControl(change) {
    var card = controlCard(change.id);
    if (!card) {
      return;
    }
    probes.controls += 1;
    card.setAttribute('data-state', change.state);
    var badge = card.querySelector('.e5-state-badge');
    if (badge) {
      badge.textContent = change.state;
      badge.setAttribute('data-state', change.state);
    }
    var stateNode = card.querySelector('[data-control-state="' + change.id + '"]');
    if (stateNode && change.stateText !== undefined) {
      stateNode.textContent = change.stateText;
    }
    var help = card.querySelector('#e5-desc-' + change.id);
    if (help && change.valueText !== undefined) {
      help.textContent = change.valueText + (change.source ? ' · source ' + change.source : '');
    }
    var activateButton = card.querySelector('[data-activate]');
    if (activateButton && change.disabled !== undefined) {
      activateButton.disabled = change.disabled === true;
      activateButton.setAttribute('aria-disabled', change.disabled === true ? 'true' : 'false');
    }
    if (change.value !== undefined && change.disabled !== undefined) {
      var field = card.querySelector('[data-input="' + change.id + '"]');
      var stagedHere = card.getAttribute('data-staged') === 'true';
      if (field && !stagedHere && document.activeElement !== field && field.tagName !== 'OUTPUT') {
        if (field.type === 'checkbox') {
          field.checked = change.value === true;
        } else if (field.tagName === 'SELECT') {
          field.value = typeof change.value === 'string' ? change.value : '';
        } else if (field.tagName === 'TEXTAREA') {
          field.value = Array.isArray(change.value)
            ? change.value.join('\n')
            : change.value === null || change.value === undefined
              ? ''
              : String(change.value);
        } else {
          field.value = change.value === null || change.value === undefined ? '' : String(change.value);
        }
      }
      for (var formField of card.querySelectorAll('[data-input]')) {
        if (formField.tagName !== 'OUTPUT' && formField.getAttribute('data-kind') !== 'action') {
          formField.disabled = change.disabled === true;
        }
      }
    }
    var applyState = card.querySelector('.e5-apply-state');
    if (applyState && change.applyState !== undefined) {
      applyState.textContent = change.applyState || '';
      applyState.setAttribute('data-apply-state', change.applyState || '');
    }
    if (change.options !== undefined) {
      var select = card.querySelector('select[data-input="' + change.id + '"]');
      if (select) {
        var signature = change.options.join('\u0000');
        if (select.getAttribute('data-options') !== signature) {
          select.setAttribute('data-options', signature);
          var current = select.value;
          while (select.firstChild) {
            select.removeChild(select.firstChild);
          }
          var inherited = document.createElement('option');
          inherited.value = '';
          inherited.textContent = 'Inherit (not configured)';
          select.appendChild(inherited);
          for (var option of change.options) {
            var node = document.createElement('option');
            node.value = option;
            node.textContent = option;
            select.appendChild(node);
          }
          select.value = current;
          probes.options += 1;
        }
      }
    }
  }

  function updateRows(screenId, rows) {
    var screen = screensById.get(screenId);
    if (!screen) {
      return;
    }
    var host = screen.querySelector('.e5-data');
    if (!host) {
      if (!rows || rows.length === 0) {
        return;
      }
      host = document.createElement('section');
      host.className = 'e5-data';
      host.setAttribute('aria-label', 'Served ' + screenId + ' state');
      screen.insertBefore(host, screen.querySelector('.e5-group'));
    }
    var existing = new Map();
    for (var node of host.querySelectorAll('[data-row-key]')) {
      existing.set(node.getAttribute('data-row-key'), node);
    }
    var seen = new Set();
    for (var row of rows) {
      seen.add(row.key);
      var line = existing.get(row.key);
      if (!line) {
        line = document.createElement('div');
        line.className = 'e5-row';
        line.setAttribute('data-row-key', row.key);
        var label = document.createElement('span');
        label.className = 'e5-row-label';
        var value = document.createElement('span');
        value.className = 'e5-row-value';
        line.appendChild(label);
        line.appendChild(value);
        host.appendChild(line);
      }
      var signature = row.label + '\u0000' + row.value + '\u0000' + row.state;
      if (line.getAttribute('data-signature') === signature) {
        continue;
      }
      line.setAttribute('data-signature', signature);
      line.setAttribute('data-row-state', row.state);
      var labelNode = line.querySelector('.e5-row-label');
      var valueNode = line.querySelector('.e5-row-value');
      if (labelNode) {
        labelNode.textContent = row.label;
      }
      if (valueNode) {
        valueNode.textContent = row.value;
        valueNode.setAttribute('data-state', row.state);
      }
      probes.rows += 1;
    }
    for (var [key, node2] of existing) {
      if (!seen.has(key)) {
        node2.remove();
        probes.rows += 1;
      }
    }
  }

  function applyPatch(message) {
    probes.patches += 1;
    if (message.revision) {
      var revision = document.querySelector('[data-field="revision"]');
      if (revision) {
        revision.textContent = 'Config r' + message.revision;
      }
    }
    if (message.stale !== undefined) {
      app.setAttribute('data-stale', message.stale ? 'true' : 'false');
      if (message.staleText) {
        setNotice(message.staleText);
      }
    }
    for (var change of message.controls || []) {
      updateControl(change);
    }
    for (var [screenId, rows] of Object.entries(message.rows || {})) {
      updateRows(screenId, rows);
    }
    if (message.notice) {
      setNotice(message.notice);
    }
    applyInFlight = false;
    footerPatch();
  }

  window.addEventListener('message', function (event) {
    var message = event.data;
    if (!message || typeof message.type !== 'string') {
      return;
    }
    switch (message.type) {
      case 'e5Draft':
        applyDraft(message);
        if (message.error) {
          setNotice(message.error);
        } else if (message.notice) {
          setNotice(message.notice);
        }
        return;
      case 'e5Patch':
        applyPatch(message);
        return;
      case 'e5Notice':
        setNotice(message.message || '');
        return;
      case 'e5Error':
        setApplyState(message.message || 'The request failed.', 'error');
        applyInFlight = false;
        footerPatch();
        return;
      case 'e5ActionResult':
        applyInFlight = false;
        setApplyState(message.message || 'Daemon acknowledged.', message.ok === false ? 'error' : 'info');
        if (message.ok === false) {
          setNotice(message.message || 'The daemon refused the request.');
        }
        footerPatch();
        return;
      case 'e5SelectScreen':
        selectScreen(message.screen, false);
        return;
      default:
        return;
    }
  });

  var hash = window.location.hash.replace('#', '');
  var initial = screensById.has(hash) ? hash : null;
  if (initial === null) {
    var visible = document.querySelector('[data-screen]:not([hidden])');
    initial = visible ? visible.getAttribute('data-screen') : null;
  }
  if (initial) {
    selectScreen(initial, false);
  }
  vscode.postMessage({ type: 'e5Ready' });
})();
