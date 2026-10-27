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
  var flashTimer = null;
  var confirmTimers = new Map();
  var gPending = 0;
  var probes = { patches: 0, controls: 0, rows: 0, fullRebuilds: 0, text: 0 };
  window.__faktorE8Probe = probes;

  function setNotice(text, level) {
    noticeText = text || '';
    var flash = document.getElementById('e8-flash');
    if (!flash) return;
    flash.textContent = noticeText;
    flash.setAttribute('data-level', level || 'info');
    flash.hidden = noticeText.length === 0;
    if (flashTimer !== null) {
      clearTimeout(flashTimer);
      flashTimer = null;
    }
    if (noticeText.length > 0 && level !== 'error') {
      flashTimer = setTimeout(function () {
        flash.hidden = true;
        flashTimer = null;
      }, 10000);
    }
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
      help.textContent = patch.valueText === 'Inherit (not configured)' ? '' : patch.valueText;
      probes.text += 1;
    }
    var select = card.querySelector('select[data-input="' + patch.id + '"]');
    if (select && Array.isArray(patch.options)) {
      var current = select.value;
      select.textContent = '';
      var inherit = document.createElement('option');
      inherit.value = '';
      inherit.textContent = 'Inherit (not configured)';
      select.appendChild(inherit);
      for (var option of patch.options) {
        var choice = document.createElement('option');
        choice.value = option;
        choice.textContent = option;
        select.appendChild(choice);
      }
      var wanted = typeof patch.value === 'string' ? patch.value : current;
      if (wanted !== '' && patch.options.indexOf(wanted) < 0) {
        var served = document.createElement('option');
        served.value = wanted;
        served.textContent = wanted + ' (served value)';
        served.selected = true;
        select.appendChild(served);
      } else if (patch.options.indexOf(wanted) >= 0) {
        select.value = wanted;
      } else if (current === '' || patch.options.indexOf(current) >= 0) {
        select.value = current;
      }
    }
    var activate = card.querySelector('[data-activate="' + patch.id + '"]');
    if (activate) {
      activate.disabled = patch.disabled;
      activate.setAttribute('aria-disabled', patch.disabled ? 'true' : 'false');
    }
    var primary = document.querySelector('[data-primary-control="' + patch.id + '"]');
    if (primary) {
      primary.disabled = patch.disabled;
      primary.setAttribute('aria-disabled', patch.disabled ? 'true' : 'false');
      if (patch.stateText) primary.title = patch.stateText;
    }
    var inputs = card.querySelectorAll('[data-input="' + patch.id + '"]');
    for (var input of inputs) input.disabled = patch.disabled;
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
          var textNode = status.querySelector('[data-status-text]');
          if (textNode) textNode.textContent = message.status;
          else status.textContent = message.status;
          status.setAttribute('data-level', message.statusLevel || 'quiet');
        }
      }
    }
  }

  function display(value) {
    if (typeof value === 'string') return value.length > 80 ? value.slice(0, 77) + '\u2026' : value;
    if (Array.isArray(value)) {
      var joined = value.map(display).join(', ');
      return joined.length > 80 ? joined.slice(0, 77) + '\u2026' : joined;
    }
    if (value !== null && typeof value === 'object') {
      var fields = Object.keys(value).length;
      return fields + (fields === 1 ? ' configured field' : ' configured fields');
    }
    return String(value);
  }

  function hasChanges() {
    return staged.size > 0 || unstaged.size > 0;
  }

  function selectScreen(screenId, moveFocus) {
    var screen = screens.get(screenId);
    if (!screen) return false;
    // Config descriptors stay on the home rows; action/diagnostic pages open
    // as one full settings page and Back returns to the same search.
    var settingsHome = document.getElementById('e8-settings-home');
    var settingsBack = document.getElementById('e8-settings-back');
    if (settingsHome) settingsHome.hidden = true;
    if (settingsBack) settingsBack.hidden = false;
    selected = screenId;
    var destination = screen.getAttribute('data-destination');
    for (var [key, panel] of destinations) panel.hidden = key !== destination;
    for (var [id, node] of screens) node.hidden = id !== screenId;
    for (var [linkId, linkNode] of links) linkNode.setAttribute('aria-current', linkId === screenId ? 'page' : 'false');
    for (var tab of tabs) {
      tab.setAttribute('aria-current', tab.getAttribute('data-destination') === destination ? 'page' : 'false');
    }
    var browser = screen.closest('.e8-destination');
    if (browser) {
      for (var openBrowser of browser.querySelectorAll('.e8-browser[open]')) openBrowser.open = false;
    }
    footerPatch();
    var heading = document.getElementById('e8-screen-title-' + screenId);
    if (moveFocus && heading) {
      heading.focus({ preventScroll: true });
      window.scrollTo({ top: 0, behavior: 'auto' });
    }
    vscode.postMessage({ type: 'e8SelectScreen', screen: screenId });
    return true;
  }

  function controlLabel(controlId) {
    var row = document.getElementById('e8-control-' + controlId);
    if (row) {
      var label = row.querySelector('.e8-settings-label');
      if (label && label.textContent) return label.textContent.trim();
      var name = row.querySelector('.e8-control-name');
      if (name && name.textContent) return name.textContent.trim();
    }
    var input = document.querySelector('[data-input="' + controlId + '"]');
    return input && input.getAttribute('data-label') ? input.getAttribute('data-label') : 'Setting';
  }

  function footerPatch() {
    var list = document.getElementById('e8-patch-list');
    var count = document.getElementById('e8-staged-count');
    var footer = document.getElementById('e8-footer');
    if (!list || !count || !footer) return;
    var entries = Array.from(staged.values());
    var changes = entries.length > 0 || unstaged.size > 0;
    count.textContent = !changes
      ? 'No staged changes'
      : entries.length + ' staged \u00b7 ' + unstaged.size + ' reset';
    footer.hidden = !(changes || applyInFlight);
    var signature = entries
      .map(function (entry) { return entry.controlId + '=' + JSON.stringify(entry.value); })
      .concat(Array.from(unstaged).map(function (id) { return id + '=unset'; }))
      .sort()
      .join('|');
    if (list.getAttribute('data-signature') !== signature) {
      list.setAttribute('data-signature', signature);
      list.textContent = '';
      for (var entry of entries.slice().sort(function (a, b) { return a.controlId.localeCompare(b.controlId); })) {
        var chip = document.createElement('span');
        chip.className = 'e8-chip';
        chip.textContent = controlLabel(entry.controlId) + ' = ' + display(entry.value);
        list.appendChild(chip);
      }
      for (var id of Array.from(unstaged).sort()) {
        var chip2 = document.createElement('span');
        chip2.className = 'e8-chip e8-chip-reset';
        chip2.textContent = controlLabel(id) + ' \u00b7 reset to inherited';
        list.appendChild(chip2);
      }
    }
    var screen = selected === null ? null : screens.get(selected);
    var primaryApply = screen ? screen.querySelector('[data-primary-apply]') : null;
    var footerApply = document.getElementById('e8-apply');
    if (primaryApply) {
      primaryApply.disabled = applyInFlight || !changes;
      primaryApply.setAttribute('aria-disabled', primaryApply.disabled ? 'true' : 'false');
      if (footerApply) footerApply.hidden = true;
    } else if (footerApply) {
      footerApply.hidden = false;
      footerApply.disabled = applyInFlight || !changes;
    }
    var reset = document.getElementById('e8-reset');
    if (reset) reset.disabled = applyInFlight || !changes;
    var defaults = document.getElementById('e8-defaults');
    if (defaults) defaults.disabled = applyInFlight;
    var saveState = document.getElementById('e8-settings-save-state');
    if (saveState) {
      if (applyInFlight) {
        saveState.textContent = 'Applying one atomic patch\u2026';
        saveState.setAttribute('data-state', 'saving');
      } else if (changes) {
        var total = entries.length + unstaged.size;
        saveState.textContent = total + (total === 1 ? ' change' : ' changes') +
          ' staged \u2014 not saved until Apply changes.';
        saveState.setAttribute('data-state', 'dirty');
      } else {
        saveState.textContent = 'Autosave is off \u2014 no staged changes.';
        saveState.setAttribute('data-state', 'clean');
      }
    }
  }

  function applyDraft(message) {
    if (Array.isArray(message.set)) {
      staged = new Map();
      for (var entry of message.set) staged.set(entry.controlId, entry);
    }
    if (Array.isArray(message.unset)) unstaged = new Set(message.unset);
    if (message.error) setNotice(message.error, 'error');
    if (message.notice) setNotice(message.notice, 'info');
    footerPatch();
  }

  /**
   * One searchable settings surface: filters the domain rows (and their
   * advanced disclosure) from the rendered catalog only. No daemon read is
   * triggered by typing.
   */
  function filterSettingsHome(input) {
    var home = document.getElementById('e8-settings-home');
    if (!home) return;
    var query = input.value.trim().toLowerCase();
    var rows = home.querySelectorAll('[data-settings-row], [data-settings-link-row]');
    var descriptors = home.querySelectorAll('[data-settings-row]').length;
    var visible = 0;
    for (var i = 0; i < rows.length; i++) {
      var row = rows[i];
      var haystack = row.getAttribute('data-settings-haystack') || '';
      var match = query.length === 0 || haystack.indexOf(query) !== -1;
      row.hidden = !match;
      if (match) visible += 1;
    }
    var domains = home.querySelectorAll('[data-settings-domain]');
    for (var d = 0; d < domains.length; d++) {
      var domainRows = domains[d].querySelectorAll('[data-settings-row], [data-settings-link-row]');
      var shown = 0;
      for (var r = 0; r < domainRows.length; r++) if (!domainRows[r].hidden) shown += 1;
      domains[d].hidden = shown === 0;
      for (var details of domains[d].querySelectorAll('[data-settings-advanced], [data-settings-actions]')) {
        var disclosed = 0;
        for (var advancedRow of details.querySelectorAll('[data-settings-row], [data-settings-link-row]')) {
          if (!advancedRow.hidden) disclosed += 1;
        }
        if (query.length > 0 && disclosed > 0) details.open = true;
      }
    }
    var count = home.querySelector('[data-settings-global-count]');
    if (count) {
      count.textContent = query.length === 0
        ? descriptors + ' settings'
        : visible + (visible === 1 ? ' setting matches' : ' settings match');
    }
    var empty = home.querySelector('[data-settings-empty]');
    if (empty) empty.hidden = visible !== 0;
  }

  function showSettingsHome() {
    var home = document.getElementById('e8-settings-home');
    if (home) home.hidden = false;
    for (var panel of Array.from(document.querySelectorAll('.e8-screen'))) panel.hidden = true;
    var destination = document.querySelector('[data-destination-panel="settings"]');
    if (destination) destination.hidden = true;
    var back = document.getElementById('e8-settings-back');
    if (back) back.hidden = true;
    selected = null;
    var search = document.getElementById('e8-settings-search');
    if (search) search.focus();
    if (home) home.scrollIntoView({ block: 'start' });
  }

  function installSettingsHome() {
    var input = document.getElementById('e8-settings-search');
    if (input && input.dataset.bound !== 'true') {
      input.dataset.bound = 'true';
      input.addEventListener('input', function () { filterSettingsHome(input); });
      input.addEventListener('search', function () { filterSettingsHome(input); });
      filterSettingsHome(input);
    }
    var rows = document.querySelectorAll('[data-settings-open]');
    for (var i = 0; i < rows.length; i++) {
      if (rows[i].dataset.bound === 'true') continue;
      rows[i].dataset.bound = 'true';
      rows[i].addEventListener('click', function (event) {
        var button = event.currentTarget;
        selectScreen(button.getAttribute('data-settings-open'), true);
      });
    }
    var back = document.getElementById('e8-settings-back');
    if (back && back.dataset.bound !== 'true') {
      back.dataset.bound = 'true';
      back.addEventListener('click', showSettingsHome);
    }
    var review = document.querySelector('[data-settings-review]');
    if (review && review.dataset.bound !== 'true') {
      review.dataset.bound = 'true';
      review.addEventListener('click', function () {
        var footer = document.getElementById('e8-footer');
        if (footer) {
          footer.scrollIntoView({ block: 'nearest' });
          var apply = document.getElementById('e8-apply');
          if (apply && !apply.disabled) apply.focus();
        }
      });
    }
  }

  function filterSettings(input) {
    var screen = input.closest('.e8-screen');
    if (!screen) return;
    var query = input.value.trim().toLowerCase();
    var cards = screen.querySelectorAll('.e8-control');
    var shown = 0;
    for (var i = 0; i < cards.length; i++) {
      var card = cards[i];
      if (card.querySelector('[data-settings-search]')) continue;
      var text = (card.textContent || '').toLowerCase();
      var match = query.length === 0 || text.indexOf(query) !== -1;
      card.hidden = !match;
      if (match) shown += 1;
    }
    var rows = screen.querySelectorAll('.e8-row');
    for (var j = 0; j < rows.length; j++) {
      var row = rows[j];
      row.hidden = query.length > 0 && (row.textContent || '').toLowerCase().indexOf(query) === -1;
    }
    var count = screen.querySelector('[data-settings-count]');
    if (count) {
      count.textContent = query.length === 0
        ? 'Searches the served catalog on this screen; no daemon round trip is needed.'
        : shown + ' control' + (shown === 1 ? '' : 's') + ' and ' + Array.prototype.filter.call(rows, function (row) { return !row.hidden; }).length + ' served row(s) match.';
    }
  }

  function installSettingsSearch() {
    var inputs = document.querySelectorAll('[data-settings-search]');
    for (var i = 0; i < inputs.length; i++) {
      if (inputs[i].dataset.bound === 'true') continue;
      inputs[i].dataset.bound = 'true';
      inputs[i].addEventListener('input', function (event) { filterSettings(event.currentTarget); });
      inputs[i].addEventListener('search', function (event) { filterSettings(event.currentTarget); });
      filterSettings(inputs[i]);
    }
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

  function activate(id, extra, scope) {
    var payload = actionPayload(id);
    // Q-06 exists on more than one rendered screen; only the controls in the
    // screen that owns the clicked button may supply the request id.
    var screen = scope && scope.closest ? scope.closest('.e8-screen') : null;
    var find = function (selector) {
      return screen ? screen.querySelector(selector) : document.querySelector(selector);
    };
    var requestField = find('[data-approval-request="' + id + '"]');
    var decisionField = find('[data-approval-decision="' + id + '"]');
    if (requestField) {
      // A decision must name the exact live request rendered in THIS list;
      // there is no "first pending" fallback and no textarea override.
      if (!requestField.value) {
        setNotice('Choose the exact pending request before sending a decision. Nothing was sent.', 'error');
        requestField.focus();
        return;
      }
      payload.permission_id = requestField.value;
      delete payload.request_id;
      delete payload.id;
    }
    if (extra && extra.decision) {
      payload.decision = extra.decision;
    } else if (decisionField) {
      if (!decisionField.value) {
        setNotice('Choose Allow once or Deny before sending a decision. Nothing was sent.', 'error');
        decisionField.focus();
        return;
      }
      payload.decision = decisionField.value;
    }
    if (extra) {
      for (var key of Object.keys(extra)) {
        if (key !== 'decision') payload[key] = extra[key];
      }
    }
    vscode.postMessage({ type: 'e8Activate', controlId: id, payload: payload });
  }

  function clearConfirm(button) {
    var timer = confirmTimers.get(button);
    if (timer !== undefined) {
      clearTimeout(timer);
      confirmTimers.delete(button);
    }
    if (button.getAttribute('data-confirm-pending') === 'true') {
      var original = button.getAttribute('data-original-label');
      if (original !== null) button.textContent = original;
      button.removeAttribute('data-original-label');
      button.removeAttribute('data-confirm-pending');
    }
  }

  function clearAllConfirms() {
    for (var button of Array.from(confirmTimers.keys())) clearConfirm(button);
    for (var pending of Array.from(document.querySelectorAll('[data-confirm-pending="true"]'))) clearConfirm(pending);
  }

  function confirmThen(button, action) {
    if (button.getAttribute('data-confirm-pending') === 'true') {
      clearConfirm(button);
      action();
      return;
    }
    if (button.getAttribute('data-confirm') !== null) {
      button.setAttribute('data-original-label', button.textContent || '');
      button.setAttribute('data-confirm-pending', 'true');
      button.textContent = 'Confirm: ' + (button.getAttribute('data-confirm-label') || 'Continue');
      setNotice('Review the effect, then click again to confirm. Nothing has been sent yet.', 'warning');
      var timer = setTimeout(function () { clearConfirm(button); }, 8000);
      confirmTimers.set(button, timer);
      return;
    }
    action();
  }

  function flushActiveInput() {
    var active = document.activeElement;
    if (active && active.getAttribute && active.getAttribute('data-input')) {
      active.dispatchEvent(new Event('change', { bubbles: true }));
    }
  }

  function postApply() {
    if (applyInFlight || !hasChanges()) return;
    flushActiveInput();
    applyInFlight = true;
    footerPatch();
    setNotice('Applying staged changes; the draft is preserved until the daemon acknowledges.', 'info');
    vscode.postMessage({
      type: 'e8Apply',
      set: Array.from(staged.values()),
      unset: Array.from(unstaged),
    });
  }

  function openBrowser(focusSearch) {
    if (selected === null) return;
    var screen = screens.get(selected);
    if (!screen) return;
    var destination = screen.closest('.e8-destination');
    var browser = destination ? destination.querySelector('.e8-browser') : null;
    if (!browser) return;
    browser.open = true;
    if (focusSearch) {
      var filter = browser.querySelector('[data-screen-filter]');
      if (filter) filter.focus();
    }
  }

  function filterScreens(value) {
    var query = value.trim().toLowerCase();
    var destination = selected === null ? null : screens.get(selected)?.closest('.e8-destination');
    if (!destination) destination = document.querySelector('.e8-destination');
    if (!destination) return;
    var any = false;
    for (var group of destination.querySelectorAll('[data-screen-group]')) {
      var shown = 0;
      for (var link of group.querySelectorAll('[data-screen-link]')) {
        var haystack = (link.getAttribute('data-screen-search') || link.textContent || '').toLowerCase();
        var match = query.length === 0 || haystack.indexOf(query) >= 0;
        link.hidden = !match;
        if (match) shown += 1;
      }
      group.hidden = shown === 0;
      if (shown > 0) any = true;
    }
    var empty = destination.querySelector('.e8-filter-empty');
    if (empty) empty.hidden = any;
  }

  document.addEventListener('click', function (event) {
    var target = event.target;
    if (!(target instanceof Element)) return;
    var link = target.closest('[data-screen-link]');
    if (link) {
      event.preventDefault();
      selectScreen(link.getAttribute('data-screen-link'), true);
      return;
    }
    var open = target.closest('[data-open-screen]');
    if (open) {
      event.preventDefault();
      var targetScreen = open.getAttribute('data-open-screen');
      if (!screens.has(targetScreen)) {
        vscode.postMessage({ type: 'e8OpenScreen', screen: targetScreen });
      } else {
        selectScreen(targetScreen, true);
      }
      return;
    }
    var tab = target.closest('.e8-tab');
    if (tab) {
      event.preventDefault();
      if (tab.getAttribute('aria-current') === 'page') return;
      var destination = tab.getAttribute('data-destination');
      var first = document.querySelector('[data-destination-panel="' + destination + '"] [data-screen]');
      if (first) selectScreen(first.getAttribute('data-screen'), true);
      return;
    }
    var primaryApply = target.closest('[data-primary-apply]');
    if (primaryApply) {
      event.preventDefault();
      postApply();
      return;
    }
    var primaryControl = target.closest('[data-primary-control]');
    if (primaryControl) {
      event.preventDefault();
      var primaryId = primaryControl.getAttribute('data-primary-control');
      var decision = primaryControl.getAttribute('data-primary-decision');
      confirmThen(primaryControl, function () {
        activate(primaryId, decision ? { decision: decision } : {}, primaryControl);
      });
      return;
    }
    var activateButton = target.closest('[data-activate]');
    if (activateButton) {
      event.preventDefault();
      var activateId = activateButton.getAttribute('data-activate');
      confirmThen(activateButton, function () { activate(activateId, null, activateButton); });
      return;
    }
    var inspect = target.closest('[data-inspect]');
    if (inspect) {
      event.preventDefault();
      var inspectId = inspect.getAttribute('data-inspect');
      var inspectCard = controlCard(inspectId);
      if (inspectCard) {
        var details = inspectCard.querySelector('.e8-control-details');
        if (details) details.open = true;
        var reason = inspectCard.querySelector('.e8-control-state');
        if (reason) reason.scrollIntoView({ block: 'nearest' });
      }
      return;
    }
    var unstageButton = target.closest('[data-unstage]');
    if (unstageButton) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Unstage', controlId: unstageButton.getAttribute('data-unstage') });
      return;
    }
    var hostCommand = target.closest('[data-host-command]');
    if (hostCommand) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8HostCommand', command: hostCommand.getAttribute('data-host-command') });
      return;
    }
    if (target.closest('#e8-refresh') || target.closest('[data-retry]')) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Refresh' });
      return;
    }
    if (target.closest('#e8-reset')) {
      event.preventDefault();
      vscode.postMessage({ type: 'e8Reset' });
      return;
    }
    var defaults = target.closest('#e8-defaults');
    if (defaults) {
      event.preventDefault();
      confirmThen(defaults, function () {
        vscode.postMessage({ type: 'e8Defaults' });
      });
      return;
    }
    if (target.closest('#e8-apply')) {
      event.preventDefault();
      postApply();
      return;
    }
  });

  document.addEventListener('input', function (event) {
    var target = event.target;
    if (!(target instanceof Element)) return;
    if (target.getAttribute('data-screen-filter') !== null) {
      filterScreens(target.value || '');
    }
  });

  document.addEventListener('keydown', function (event) {
    var active = document.activeElement;
    var tag = active && active.tagName ? active.tagName : '';
    var typing = tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || (active && active.isContentEditable === true);

    if (event.key === 'k' && (event.metaKey || event.ctrlKey) && !event.altKey) {
      event.preventDefault();
      openBrowser(true);
      return;
    }
    if (event.key === 'Enter' && (event.metaKey || event.ctrlKey)) {
      if (hasChanges()) {
        event.preventDefault();
        postApply();
      }
      return;
    }
    if (event.key === 'Escape') {
      var pending = document.querySelector('[data-confirm-pending="true"]');
      if (pending) {
        clearAllConfirms();
        event.preventDefault();
        setNotice('Confirmation cancelled. Nothing was sent.', 'info');
        return;
      }
      var openDetails = Array.from(document.querySelectorAll('details[open]')).filter(function (node) {
        return node.getClientRects().length > 0;
      });
      if (openDetails.length > 0) {
        var top = openDetails[openDetails.length - 1];
        top.open = false;
        var summary = top.querySelector(':scope > summary');
        if (summary) summary.focus({ preventScroll: true });
        event.preventDefault();
      }
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey) {
      if (event.key === 'r' || event.key === 'R') {
        event.preventDefault();
        vscode.postMessage({ type: 'e8Refresh' });
        return;
      }
      var index = ['1', '2', '3', '4'].indexOf(event.key);
      if (index >= 0 && tabs[index]) {
        event.preventDefault();
        tabs[index].click();
        return;
      }
    }
    if (!typing && !event.metaKey && !event.ctrlKey && !event.altKey) {
      if (event.key === 'g') {
        gPending = Date.now();
        return;
      }
      if (gPending > 0 && Date.now() - gPending < 1200) {
        var destination = event.key === 'w' ? 'work' : event.key === 'i' ? 'inspect' : event.key === 'h' ? 'history' : event.key === 's' ? 'settings' : '';
        gPending = 0;
        if (destination.length > 0) {
          event.preventDefault();
          var destinationTab = tabs.find(function (tab) { return tab.getAttribute('data-destination') === destination; });
          if (destinationTab) {
            destinationTab.click();
          } else if (destination === 'settings') {
            vscode.postMessage({ type: 'e8OpenScreen', screen: 'S29' });
          }
        }
        return;
      }
    }
    if (event.key === 'Enter' || event.key === ' ') {
      var openRow = document.activeElement;
      if (openRow && openRow.getAttribute && openRow.getAttribute('data-open-screen')) {
        event.preventDefault();
        var rowScreen = openRow.getAttribute('data-open-screen');
        if (screens.has(rowScreen)) selectScreen(rowScreen, true);
      }
    }
  });

  // Stage on change; never on every keystroke (input latency and IME safety).
  document.addEventListener('change', function (event) {
    var target = event.target;
    if (!(target instanceof Element)) return;
    var id = target.getAttribute && target.getAttribute('data-input');
    if (!id || id === 'O-01') return;
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
        installSettingsSearch();
        installSettingsHome();
        return;
      case 'e8Notice':
        setNotice(message.message || '', 'info');
        return;
      case 'e8Header': {
        var header = document.getElementById('e8-header-status');
        if (header) {
          var level = typeof message.level === 'string' ? message.level : 'quiet';
          var glyph = document.createElement('span');
          glyph.setAttribute('aria-hidden', 'true');
          glyph.textContent = level === 'error' ? '!' : level === 'waiting' ? '\u2026' : '\u25cf';
          header.setAttribute('data-level', level);
          header.replaceChildren(glyph, document.createTextNode(' ' + String(message.status || '')));
        }
        return;
      }
      case 'e8Error':
        setNotice(message.message || '', 'error');
        return;
      case 'e8ActionResult':
        if (applyInFlight) {
          applyInFlight = false;
          footerPatch();
        }
        setNotice(message.message || '', message.ok === false ? 'error' : 'info');
        return;
      case 'e8SettingsHome':
        showSettingsHome();
        return;
      case 'e8SelectScreen':
        if (typeof message.screen === 'string' && screens.has(message.screen)) selectScreen(message.screen, false);
        return;
      default:
        return;
    }
  });

  // Settings boots on its one searchable surface, not on whichever screen
  // happens to be the command's initial screen.
  if (window.__faktorE8Initial && !document.getElementById('e8-settings-home')) {
    selectScreen(window.__faktorE8Initial, false);
  }
  footerPatch();
  vscode.postMessage({ type: 'e8Ready' });
  installSettingsSearch();
  installSettingsHome();
})();
