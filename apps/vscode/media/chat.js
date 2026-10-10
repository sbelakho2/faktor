// Faktor chat webview script. Hand-written, dependency-free, no remote
// loads, no eval: it renders the snapshot the extension posts and sends
// typed commands back. All daemon-derived strings go through textContent.
// Composer draft policy comes from media/composer-state.js (loaded first).
(function () {
  'use strict';

  var vscode = acquireVsCodeApi();
  var composerPolicy = (typeof FaktorComposer !== 'undefined' && FaktorComposer) || {
    afterStart: function (draft) {
      return String(draft == null ? '' : draft);
    },
  };
  // Board presentation policy comes from media/board-state.js (loaded first);
  // the fallback keeps the panel functional without it but never fabricates.
  var boardPolicy = (typeof FaktorBoard !== 'undefined' && FaktorBoard) || {
    MAX_BOARD_SUBJECT_BYTES: 512,
    MAX_BOARD_BODY_BYTES: 16 * 1024,
    boardHeader: function (board) {
      return board == null ? 'board: not read yet' : 'board: unavailable (policy missing)';
    },
    boardDraftRefusal: function (subject, body) {
      if (String(subject == null ? '' : subject).trim().length === 0) {
        return 'subject is required';
      }
      if (String(body == null ? '' : body).trim().length === 0) {
        return 'body is required';
      }
      return null;
    },
    boardPostsForDisplay: function () {
      return [];
    },
  };

  // One logical submission at a time. The flag gates the composer BEFORE the
  // post, so a double click / Enter+click race can never emit a second
  // sendGoal; it is released ONLY by an explicit host result.
  var submitting = false;
  /** True while a board post awaits its durable ack (double-click guard). */
  var boardPosting = false;
  /** The exact draft a board post submitted (token-correlated ack). */
  var submittedBoard = null;
  var boardSubmitSeq = 0;
  /** True while an IME composition is active (chord/Enter must not submit). */
  var composing = false;
  // Scroll-ownership state: the signature of the rendered transcript and the
  // one-shot pin requested by the user's own submission.
  var transcriptKey = null;
  var transcriptEntryKeys = null;
  var transcriptPinRequested = false;
  var TRANSCRIPT_PIN_SLACK_PX = 24;
  // Host-side attachment metadata (id + filename + mime + size only: the
  // host owns the bytes). Paste/drop bytes transit once through the webview,
  // bounded; a picker selection never ships base64 through here.
  var attachments = [];
  var currentSessionId = null;
  var streamBlockedReason = null;
  var streamBlockedCursor = null;
  /** The last raw snapshot error rendered, so it is not re-announced. */
  var lastRenderedError = null;
  /** Technical tails currently displayed; identical errors never stack. */
  var shownUserErrors = [];
  var MAX_USER_ERRORS = 3;
  var MAX_PENDING_PERMISSIONS = 16;
  var WEBVIEW_MAX_ATTACHMENT_BYTES = 7 * 1024 * 1024;
  var WEBVIEW_MAX_ATTACHMENTS = 8;
  var B64_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

  function byId(id) {
    return document.getElementById(id);
  }

  /**
   * 128-bit content digest: four independent FNV-1a lanes. A 32-bit lane is
   * cheaply collidable (a provider-controlled same-length string with the
   * same hash would freeze the DOM), so per-entry identity uses 128 bits and
   * the streaming tuple compare backs it up.
   */
  function hash128(text) {
    var value = text == null ? '' : String(text);
    var seeds = [2166136261, 2166136261 ^ 0x9e3779b9, 2166136261 ^ 0x85ebca6b, 2166136261 ^ 0xc2b2ae35];
    var out = '';
    for (var lane = 0; lane < seeds.length; lane++) {
      var h = seeds[lane] >>> 0;
      for (var i = 0; i < value.length; i++) {
        h ^= value.charCodeAt(i);
        h = Math.imul(h, 16777619) >>> 0;
      }
      out += (h.toString(16) + '00000000').slice(0, 8);
    }
    return out;
  }

  /** Child-index path from `root` to `node`; null when not a descendant. */
  function elementPath(root, node) {
    var path = [];
    var cursor = node;
    while (cursor && cursor !== root) {
      var parent = cursor.parentNode;
      if (!parent || !parent.children) {
        return null;
      }
      var index = -1;
      for (var i = 0; i < parent.children.length; i++) {
        if (parent.children[i] === cursor) {
          index = i;
          break;
        }
      }
      if (index < 0) {
        return null;
      }
      path.unshift(index);
      cursor = parent;
    }
    return cursor === root ? path : null;
  }

  function elementAtPath(root, path) {
    var cursor = root;
    for (var i = 0; i < path.length; i++) {
      cursor = cursor && cursor.children ? cursor.children[path[i]] : null;
      if (!cursor) {
        return null;
      }
    }
    return cursor;
  }

  /**
   * Capture the focused control inside a container across a rebuild. The
   * structural path is the primary identity: duplicate visible labels AND
   * duplicate aria-labels (two "Remove" buttons, two "Pause" buttons) must
   * keep focus on the SAME control, which text/aria matching alone cannot.
   */
  function captureFocus(container) {
    var active = typeof document !== 'undefined' ? document.activeElement : null;
    if (!active || !container || !container.contains || !container.contains(active)) {
      return null;
    }
    return {
      path: elementPath(container, active),
      tag: active.tagName,
      text: active.textContent,
      aria: active.getAttribute ? active.getAttribute('aria-label') : null,
    };
  }

  function restoreFocus(container, descriptor) {
    if (!descriptor || !container || !container.querySelectorAll) {
      return;
    }
    if (descriptor.path) {
      var exact = elementAtPath(container, descriptor.path);
      if (exact && exact.tagName === descriptor.tag && typeof exact.focus === 'function') {
        exact.focus();
        return;
      }
    }
    var nodes = container.querySelectorAll('button, input, textarea, select, a, [tabindex]');
    for (var i = 0; i < nodes.length; i++) {
      var node = nodes[i];
      var match =
        descriptor.aria !== null
          ? node.getAttribute && node.getAttribute('aria-label') === descriptor.aria
          : node.textContent === descriptor.text;
      if (match && node.tagName === descriptor.tag) {
        if (typeof node.focus === 'function') {
          node.focus();
        }
        return;
      }
    }
  }

  function clear(node) {
    while (node.firstChild) {
      node.removeChild(node.firstChild);
    }
  }

  function setText(id, value) {
    var node = byId(id);
    if (!node) {
      return;
    }
    if (value === null || value === undefined || value === '') {
      node.textContent = '—';
      return;
    }
    node.textContent = String(value);
  }

  function line(parent, value, className) {
    var node = document.createElement('div');
    if (className) {
      node.className = className;
    }
    node.textContent = String(value);
    parent.appendChild(node);
    return node;
  }

  function span(parent, value, className) {
    var node = document.createElement('span');
    if (className) {
      node.className = className;
    }
    node.textContent = String(value);
    parent.appendChild(node);
    return node;
  }

  // ------------------------------------------------------------- cockpit

  function renderCockpit(view, sections) {
    var node = byId('cockpit');
    var focus = captureFocus(node);
    clear(node);
    if (!Array.isArray(sections) || sections.length === 0) {
      return;
    }
    for (var i = 0; i < sections.length; i++) {
      var section = sections[i];
      var block = document.createElement('section');
      block.className = 'cockpit-section' + (section.present ? '' : ' cockpit-empty');
      var head = document.createElement('h3');
      head.textContent = section.title;
      block.appendChild(head);
      // The acceptance-criteria section renders one PROOF row per criterion
      // when the host serves the structured rows; older snapshots fall back
      // to the bounded lines (whose `[verdict]` prefix still styles).
      var criteria = Array.isArray(section.criteria) ? section.criteria : [];
      if (section.key === 'acceptance' && criteria.length > 0) {
        for (var c = 0; c < criteria.length; c++) {
          renderCriterionRow(block, criteria[c]);
        }
      } else {
        var lines = Array.isArray(section.lines) ? section.lines : [];
        for (var j = 0; j < lines.length; j++) {
          if (section.key === 'evidence') {
            renderEvidenceLine(block, section, lines[j], j);
          } else if (section.key === 'usage') {
            renderUsageLine(block, lines[j]);
          } else {
            var verdict = /^\[(pass|fail|unavailable)\]/.exec(String(lines[j]));
            line(block, lines[j], verdict ? 'criterion-line criterion-line-' + verdict[1] : 'muted');
          }
        }
      }
      // State-gated controls: a disabled action is rendered disabled and
      // NEVER posts (the server gate is authoritative). Tournament actions
      // route to the tournament engine; usage actions route to the
      // commercial-metering controls (cursor paging + role-gated grants).
      var actions = Array.isArray(section.actions) ? section.actions : [];
      if (actions.length > 0 && (section.key === 'tournament' || section.key === 'usage')) {
        var controls = document.createElement('div');
        controls.className = 'cockpit-actions';
        for (var k = 0; k < actions.length; k++) {
          (function (action, sectionKey) {
            var button = document.createElement('button');
            button.type = 'button';
            button.textContent = action.label;
            button.disabled = action.enabled !== true;
            button.addEventListener('click', function () {
              if (action.enabled !== true) {
                return;
              }
              if (sectionKey === 'tournament') {
                if (!view || !view.tournament) {
                  return;
                }
                vscode.postMessage({
                  type: 'tournamentControl',
                  tournamentId: view.tournament.id,
                  action: action.key,
                });
              } else {
                vscode.postMessage({ type: 'usageControl', action: action.key });
              }
            });
            controls.appendChild(button);
          })(actions[k], section.key);
        }
        block.appendChild(controls);
      }
      node.appendChild(block);
    }
    restoreFocus(node, focus);
  }

  // Usage/credits lines carry honest state markers: a breached quota, an
  // expired/canceled subscription or a lapsed (grace) one are styled, never
  // silently rendered as healthy numbers.
  function renderUsageLine(block, value) {
    var text = String(value);
    if (/^\[(EXCEEDED|EXPIRED|CANCELED|GRACE)\]/.test(text)) {
      line(block, text, 'usage-alert');
    } else {
      line(block, text, 'muted');
    }
  }

  function renderEvidenceLine(block, section, label, index) {
    var refs = Array.isArray(section.evidence) ? section.evidence : [];
    var ref = refs[index];
    var row = document.createElement('div');
    row.className = 'evidence';
    var text = document.createElement('span');
    text.className = 'muted';
    text.textContent = label;
    row.appendChild(text);
    if (ref && typeof ref.id === 'number' && isFinite(ref.id)) {
      var id = ref.id;
      row.setAttribute('data-evidence', String(id));
      var button = document.createElement('button');
      button.type = 'button';
      button.textContent = 'View evidence #' + id;
      button.addEventListener('click', function () {
        vscode.postMessage({ type: 'retrieveEvidence', evidenceId: id });
      });
      row.appendChild(button);
    }
    block.appendChild(row);
  }

  /**
   * ISO text of one epoch-ms stamp, or null when the value is non-finite or
   * outside the JS Date range: an unrenderable stamp is an honest absence,
   * never a RangeError that aborts the whole panel render.
   */
  function utcMs(value) {
    if (typeof value !== 'number' || !isFinite(value)) {
      return null;
    }
    var date = new Date(value);
    if (!isFinite(date.getTime())) {
      return null;
    }
    return date.toISOString();
  }

  /**
   * One acceptance-criterion PROOF row: verdict (pass/fail/unavailable,
   * each with a distinct class), requirement/origin/binding with its exact
   * reference, the proven snapshots and verification timestamps, and the
   * typed evidence refs (numeric ids retrieve on click). Missing proof
   * members render as explicit "unavailable" text — never as a pass.
   */
  function renderCriterionRow(block, row) {
    var value = row || {};
    var verdict = value.verdict === 'pass' || value.verdict === 'fail' ? value.verdict : 'unavailable';
    var card = document.createElement('div');
    card.className = 'criterion criterion-' + verdict;
    card.setAttribute('data-verdict', verdict);
    if (typeof value.binding === 'string') {
      card.setAttribute('data-binding', value.binding);
    }
    var head = document.createElement('div');
    head.className = 'criterion-head';
    var badge = document.createElement('span');
    badge.className = 'criterion-verdict criterion-verdict-' + verdict;
    badge.textContent =
      verdict === 'pass' ? 'PASS' : verdict === 'fail' ? 'FAIL' : 'UNAVAILABLE';
    head.appendChild(badge);
    var key = document.createElement('span');
    key.className = 'criterion-key';
    key.textContent =
      typeof value.criterionKey === 'string' && value.criterionKey.length > 0
        ? value.criterionKey
        : '(unnamed criterion — malformed row)';
    head.appendChild(key);
    card.appendChild(head);

    var binding = String(value.binding || 'unavailable');
    if (value.bindingSource && value.bindingSource !== 'daemon') {
      binding += ' (' + String(value.bindingSource) + ')';
    }
    if (value.bindingReference) {
      binding += ' ref ' + String(value.bindingReference);
    }
    line(
      card,
      'requirement ' + String(value.requirement || 'unavailable') +
        ' · origin ' + String(value.origin || 'unavailable') +
        ' · binding ' + binding,
      'muted criterion-meta',
    );
    if (value.bindingDetail) {
      line(card, String(value.bindingDetail), 'muted');
    }

    var snapshot = value.snapshot || {};
    var snapshotBits = [
      snapshot.candidate ? 'candidate ' + String(snapshot.candidate) : null,
      snapshot.verified ? 'verified ' + String(snapshot.verified) : null,
      snapshot.basedOn ? 'basedOn ' + String(snapshot.basedOn) : null,
      snapshot.landed ? 'landed ' + String(snapshot.landed) : null,
      typeof snapshot.sourceCount === 'number' ? 'sources ' + snapshot.sourceCount : null,
    ].filter(function (bit) {
      return bit !== null;
    });
    line(
      card,
      snapshotBits.length > 0 ? 'snapshot ' + snapshotBits.join(' · ') : 'snapshot unavailable',
      'muted criterion-snapshot',
    );

    var timestamp = value.timestamp || {};
    var atBits = [];
    var startedText = utcMs(timestamp.startedMs);
    if (startedText !== null) {
      atBits.push('started ' + startedText);
    }
    var completedText = utcMs(timestamp.completedMs);
    if (completedText !== null) {
      atBits.push('completed ' + completedText);
    }
    line(
      card,
      atBits.length > 0 ? 'verified at ' + atBits.join(' ') : 'verification timestamp unavailable',
      'muted criterion-timestamp',
    );

    var refs = Array.isArray(value.evidenceRefs) ? value.evidenceRefs : [];
    for (var r = 0; r < refs.length; r++) {
      var ref = refs[r] || {};
      var refRow = document.createElement('div');
      refRow.className = 'evidence criterion-evidence';
      var span = document.createElement('span');
      span.className = 'muted';
      span.textContent = ref.label === undefined || ref.label === null ? '' : String(ref.label);
      refRow.appendChild(span);
      if (typeof ref.id === 'number' && isFinite(ref.id)) {
        var id = ref.id;
        refRow.setAttribute('data-evidence', String(id));
        var button = document.createElement('button');
        button.type = 'button';
        button.textContent = 'View evidence #' + id;
        button.addEventListener('click', function () {
          vscode.postMessage({ type: 'retrieveEvidence', evidenceId: id });
        });
        refRow.appendChild(button);
      }
      card.appendChild(refRow);
    }

    var unavailable = Array.isArray(value.unavailable) ? value.unavailable : [];
    if (unavailable.length > 0) {
      line(card, 'unavailable: ' + unavailable.join(', '), 'warn criterion-unavailable');
    }
    block.appendChild(card);
  }

  function renderTask(task, view, agents) {
    var card = byId('task-card');
    // No task, no run strip: an empty card full of em-dashes would be pure
    // noise. The Inspect sections carry whatever metadata outlives the task.
    if (!task) {
      card.hidden = true;
      return;
    }
    card.hidden = false;
    renderRunStrip(task);
    renderTaskProgress(task);
    renderTaskTests(task);
    renderTaskAgents(agents);
    setText('task-goal', task ? task.goal : '—');
    renderCompletion(task);
  }

  /** One lifecycle bucket per run state: the strip chip is theme-colored. */
  function runStateClass(state) {
    var value = String(state == null ? '' : state).trim().toLowerCase();
    if (value === 'running' || value === 'working' || value === 'in_progress' || value === 'active') {
      return 'running';
    }
    if (
      value === 'done' ||
      value === 'complete' ||
      value === 'completed' ||
      value === 'verified_complete' ||
      value === 'succeeded'
    ) {
      return 'done';
    }
    if (value === 'failed' || value === 'error' || value === 'errored') {
      return 'failed';
    }
    if (value === 'cancelled' || value === 'canceled') {
      return 'cancelled';
    }
    if (value === 'waiting' || value === 'pending' || value === 'paused' || value === 'blocked') {
      return 'waiting';
    }
    return 'unknown';
  }

  /** The run strip: one state chip, muted chips, and the quiet goal line. */
  function renderRunStrip(task) {
    var node = byId('task-state');
    if (!node) {
      return;
    }
    var state = task ? String(task.state == null ? '' : task.state) : '';
    var bucket = runStateClass(state);
    node.textContent = state.length > 0 ? state : '—';
    node.className = 'state-chip state-' + bucket;
    node.setAttribute('data-state', bucket);
  }

  /** One compact chip in a run-meta slot; an empty value hides the slot. */
  function setRunMeta(id, text, tone) {
    var node = byId(id);
    if (!node) {
      return;
    }
    clear(node);
    if (text === null || text === undefined || String(text).length === 0) {
      node.hidden = true;
      return;
    }
    node.hidden = false;
    span(node, String(text), 'chip run-chip' + (tone ? ' run-chip-' + tone : ''));
  }

  function renderTaskProgress(task) {
    var completed = task && Array.isArray(task.completed) ? task.completed.length : 0;
    var open = task && Array.isArray(task.open) ? task.open.length : 0;
    var total = completed + open;
    if (total > 0) {
      setRunMeta('task-progress', completed + '/' + total + ' steps', 'neutral');
      return;
    }
    if (task && typeof task.phase === 'string' && task.phase.length > 0) {
      setRunMeta('task-progress', task.phase, 'neutral');
      return;
    }
    setRunMeta('task-progress', null);
  }

  function renderTaskTests(task) {
    var node = byId('task-tests');
    if (!node) {
      return;
    }
    clear(node);
    var run = task && Array.isArray(task.testsRun) ? task.testsRun.length : 0;
    var failed = task && Array.isArray(task.testsFailed) ? task.testsFailed.length : 0;
    if (run === 0 && failed === 0) {
      node.hidden = true;
      return;
    }
    node.hidden = false;
    if (run > 0) {
      span(node, 'tests ' + run, 'chip run-chip run-chip-neutral');
    }
    span(
      node,
      failed > 0 ? 'failed ' + failed : 'all passed',
      'chip run-chip run-chip-' + (failed > 0 ? 'fail' : 'pass'),
    );
  }

  function renderTaskAgents(agents) {
    var count = Array.isArray(agents) ? agents.length : 0;
    if (count === 0) {
      setRunMeta('task-agents', null);
      return;
    }
    setRunMeta('task-agents', count + (count === 1 ? ' agent' : ' agents'), 'neutral');
  }

  // ---------------------------------------------------------- permissions
  // A pending permission is an ATTENTION event, not a destination: one
  // prominent approval card in the main flow, decision-first. The headline
  // says what the run wants to do and to what; the reply handle, capability
  // key and owning conversation id stay behind "View details".

  /** Human decision headline: what the agent wants to do, and to what. */
  function permissionHeadline(permission) {
    var kind = String(permission.capability == null ? '' : permission.capability).toLowerCase();
    var what;
    if (kind.indexOf('shell') !== -1 || kind.indexOf('execute') !== -1) {
      what = 'Run a shell command';
    } else if (kind.indexOf('write') !== -1) {
      what = 'Write files in this workspace';
    } else if (kind.indexOf('read') !== -1) {
      what = 'Read files in this workspace';
    } else if (kind.indexOf('network') !== -1) {
      what = 'Use the network';
    } else if (kind.indexOf('browser') !== -1) {
      what = 'Control the browser';
    } else if (kind.indexOf('git') !== -1 || kind.indexOf('scm') !== -1) {
      what = 'Run Git operations';
    } else if (kind.indexOf('mcp') !== -1) {
      what = 'Call an external tool service';
    } else {
      what = 'Use ' + String(permission.capability == null ? 'a capability' : permission.capability);
    }
    var detail = String(permission.detail == null ? '' : permission.detail);
    // The native detail is often a JSON payload naming the exact tool,
    // path, command or destination; the most decision-relevant key wins
    // (command before path/destination before the generic tool name).
    var target = null;
    var targetKeys = ['command', 'path', 'destination', 'tool'];
    for (var k = 0; k < targetKeys.length && target === null; k++) {
      var match = new RegExp('"' + targetKeys[k] + '"\\s*:\\s*"([^"]+)"').exec(detail);
      if (match) {
        target = match[1];
      }
    }
    return {
      headline: target ? what + ': ' + target : what,
      target: target,
      detail: detail,
    };
  }

  function permissionButton(permission, decision, label, secondary) {
    var id = String(permission.id == null ? '' : permission.id);
    var button = document.createElement('button');
    button.type = 'button';
    if (secondary) {
      button.className = 'secondary';
    }
    button.textContent = label;
    button.setAttribute('aria-label', label + ' permission request ' + id);
    button.addEventListener('click', function () {
      vscode.postMessage({
        type: 'permissionReply',
        permissionId: id,
        decision: decision,
      });
    });
    return button;
  }

  /**
   * The approval card list. The card is driven by the SAME snapshot data the
   * reply path resolves against; an empty list hides the card entirely.
   */
  function renderPermissions(permissions) {
    var card = byId('permission-card');
    var list = byId('permission-list');
    if (!card || !list) {
      return;
    }
    var focus = captureFocus(list);
    clear(list);
    var items = Array.isArray(permissions) ? permissions.slice(0, MAX_PENDING_PERMISSIONS) : [];
    if (items.length === 0) {
      card.hidden = true;
      return;
    }
    card.hidden = false;
    for (var i = 0; i < items.length; i++) {
      (function (permission) {
        var info = permissionHeadline(permission);
        var id = String(permission.id == null ? '' : permission.id);
        var capability = String(permission.capability == null ? '' : permission.capability);
        var sessionId = String(permission.sessionId == null ? '' : permission.sessionId);
        var item = document.createElement('li');
        item.className = 'permission';
        item.setAttribute('data-permission-id', id);
        line(item, info.headline, 'permission-action');
        line(
          item,
          info.target ? 'Needed to verify: ' + info.target : 'Needed to continue this run.',
          'muted permission-why',
        );
        var actions = document.createElement('div');
        actions.className = 'composer-actions permission-actions';
        actions.appendChild(permissionButton(permission, 'allow', 'Allow', false));
        actions.appendChild(permissionButton(permission, 'deny', 'Deny', true));
        item.appendChild(actions);
        var details = document.createElement('details');
        details.className = 'permission-details';
        var summary = document.createElement('summary');
        summary.textContent = 'View details';
        details.appendChild(summary);
        var disclosure = document.createElement('div');
        disclosure.className = 'muted permission-diagnostics';
        disclosure.textContent =
          'Request #' + id + ' · capability ' + capability + ' · conversation ' + sessionId;
        details.appendChild(disclosure);
        if (info.detail.length > 0) {
          var raw = document.createElement('pre');
          raw.className = 'excerpt';
          raw.textContent = info.detail;
          details.appendChild(raw);
        }
        item.appendChild(details);
        list.appendChild(item);
      })(items[i]);
    }
    restoreFocus(list, focus);
  }

  /**
   * The empty/onboarding state. It exists only while the conversation is
   * empty: the first rendered entry puts it out of the way for good, and the
   * example chips feed the composer (fill + focus), never submit.
   */
  function renderWelcome(entries) {
    var node = byId('welcome');
    if (!node) {
      return;
    }
    var title = byId('transcript-title');
    var empty = !entries || entries.length === 0;
    node.hidden = !empty;
    if (title) {
      title.hidden = empty;
    }
  }

  /**
   * The durable completion-contract block. The step rows are exactly what
   * the host reports: `daemon` rows when the serving daemon exposes the
   * read, `derived` rows projected from the durable task-run state, or an
   * explicit `unavailable` (never fabricated as success).
   */
  function renderCompletion(task) {
    var node = byId('task-completion');
    if (!node) {
      return;
    }
    clear(node);
    var completion = task && task.completion ? task.completion : null;
    if (!completion) {
      return;
    }
    var head = document.createElement('div');
    head.className = 'completion-head';
    head.textContent = 'Completion contract · status source: ' + String(completion.source || 'unknown');
    node.appendChild(head);
    var steps = Array.isArray(completion.steps) ? completion.steps : [];
    for (var i = 0; i < steps.length; i++) {
      var step = steps[i];
      var detail = step.detail ? ' — ' + String(step.detail) : '';
      line(node, '[' + String(step.status) + '] ' + String(step.step) + detail, 'muted');
    }
    if (completion.reason) {
      line(node, String(completion.reason), 'warn');
    }
  }

  // -------------------------------------------------------- agent panel

  function agentButton(agent, action, label) {
    var button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.addEventListener('click', function () {
      vscode.postMessage({ type: 'agentControl', agentId: agent.agentId, action: action });
    });
    return button;
  }

  /** The durable presentation tag (absent = foreground, per the native v1 contract). */
  function presentationOf(agent) {
    return agent && agent.presentation === 'background' ? 'background' : 'foreground';
  }

  /** The foreground/background toggle: posts the EXACT next durable state. */
  function presentationButton(agent) {
    var background = presentationOf(agent) === 'background';
    var button = document.createElement('button');
    button.type = 'button';
    button.className = 'presentation-toggle';
    button.textContent = background ? 'Foreground' : 'Background';
    button.addEventListener('click', function () {
      vscode.postMessage({
        type: 'agentControl',
        agentId: agent.agentId,
        action: 'presentation',
        state: background ? 'foreground' : 'background',
      });
    });
    return button;
  }

  /** The deterministic pixel sprite as an inline SVG (no external assets). */
  function pixelSprite(agent) {
    var pixel = agent.pixel;
    if (!pixel || !pixel.avatar || !Array.isArray(pixel.avatar.pixels)) {
      return null;
    }
    var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 5 5');
    svg.setAttribute('width', '22');
    svg.setAttribute('height', '22');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('class', 'pixel ' + String(pixel.animation || 'pixel-waiting'));
    var pixels = pixel.avatar.pixels;
    for (var y = 0; y < 5; y++) {
      for (var x = 0; x < 5; x++) {
        if (pixels[y * 5 + x] !== 1) {
          continue;
        }
        var rect = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
        rect.setAttribute('x', String(x));
        rect.setAttribute('y', String(y));
        rect.setAttribute('width', '1');
        rect.setAttribute('height', '1');
        rect.setAttribute('fill', pixel.avatar.color);
        svg.appendChild(rect);
      }
    }
    var leftEye = document.createElementNS('http://www.w3.org/2000/svg', 'rect');
    leftEye.setAttribute('x', '1');
    leftEye.setAttribute('y', '1');
    leftEye.setAttribute('width', '1');
    leftEye.setAttribute('height', '1');
    leftEye.setAttribute('fill', pixel.avatar.accent);
    svg.appendChild(leftEye);
    var rightEye = leftEye.cloneNode();
    rightEye.setAttribute('x', '3');
    svg.appendChild(rightEye);
    return svg;
  }

  function joinBounded(values, limit) {
    if (!Array.isArray(values) || values.length === 0) {
      return '—';
    }
    var shown = values.slice(0, limit || 12).map(function (value) {
      return typeof value === 'string' ? value : JSON.stringify(value);
    });
    if (values.length > shown.length) {
      shown.push('… ' + (values.length - shown.length) + ' more');
    }
    return shown.join('; ');
  }

  function compactJson(value) {
    if (value === null || value === undefined) {
      return '—';
    }
    try {
      var text = JSON.stringify(value);
      return text === undefined ? '—' : text;
    } catch (error) {
      return '—';
    }
  }

  function renderAgents(agents) {
    var card = byId('agents-card');
    var list = byId('agent-list');
    var focus = captureFocus(list);
    clear(list);
    if (!agents || agents.length === 0) {
      card.hidden = true;
      return;
    }
    card.hidden = false;
    // Background children are dimmed AND grouped/tucked after every
    // foreground entry; the order derives only from the durable
    // presentation field, never from a state heuristic.
    var ordered = [];
    var backgroundCount = 0;
    var i;
    for (i = 0; i < agents.length; i++) {
      if (agents[i].kind === 'child' && presentationOf(agents[i]) === 'background') {
        backgroundCount += 1;
      } else {
        ordered.push(agents[i]);
      }
    }
    var grouped = ordered.slice();
    if (backgroundCount > 0) {
      grouped.push({ groupLabel: 'Background (' + backgroundCount + ') — dimmed', background: true });
    }
    for (i = 0; i < agents.length; i++) {
      if (agents[i].kind === 'child' && presentationOf(agents[i]) === 'background') {
        grouped.push(agents[i]);
      }
    }

    for (i = 0; i < grouped.length; i++) {
      var entry = grouped[i];
      if (entry.groupLabel) {
        var groupItem = document.createElement('li');
        groupItem.className = 'agent-group-label';
        groupItem.textContent = entry.groupLabel;
        list.appendChild(groupItem);
        continue;
      }
      var agent = entry;
      var background = agent.kind === 'child' && presentationOf(agent) === 'background';
      var item = document.createElement('li');
      item.className =
        'agent agent-' +
        (agent.kind === 'child' ? 'child' : 'self') +
        (background ? ' agent-background' : '');

      var head = document.createElement('div');
      head.className = 'agent-head';
      var sprite = pixelSprite(agent);
      if (sprite) {
        head.appendChild(sprite);
      }
      var title = document.createElement('span');
      title.className = 'agent-title';
      title.textContent =
        agent.kind +
        ' ' +
        agent.agentId +
        ' · ' +
        agent.state +
        (background ? ' · background' : '') +
        (agent.model ? ' · ' + agent.model : '') +
        (agent.provider ? ' · ' + agent.provider : '');
      head.appendChild(title);
      item.appendChild(head);

      if (agent.goal) {
        line(item, agent.goal, 'muted');
      }
      var identity = [
        agent.itemId ? 'item ' + agent.itemId + (agent.itemKind ? ' (' + agent.itemKind + ')' : '') : null,
        agent.itemIds && agent.itemIds.length > 0 ? 'items ' + agent.itemIds.join(', ') : null,
        agent.worktreeId !== null && agent.worktreeId !== undefined ? 'worktree ' + agent.worktreeId : null,
        agent.sessionId !== null && agent.sessionId !== undefined ? 'session ' + agent.sessionId : null,
        agent.ownership ? 'ownership ' + agent.ownership : null,
        agent.budget !== null && agent.budget !== undefined ? 'budget ' + agent.budget : null,
        agent.reasoning !== null && agent.reasoning !== undefined
          ? 'reasoning ' + (agent.reasoning ? 'yes' : 'no')
          : null,
        agent.thinking !== null && agent.thinking !== undefined
          ? 'thinking ' + (agent.thinking ? 'yes' : 'no')
          : null,
      ].filter(function (part) {
        return part !== null;
      });
      line(item, identity.join(' · ') || 'identity —', 'muted');
      line(item, 'capabilities: ' + joinBounded(agent.capabilities, 10), 'muted');
      line(item, 'progress: ' + compactJson(agent.progress), 'muted');
      line(item, 'result: ' + compactJson(agent.result), 'muted');
      if (agent.blockers && agent.blockers.length > 0) {
        line(item, 'blockers: ' + agent.blockers.join('; '), 'warn');
      }

      if (agent.kind === 'child') {
        var controls = document.createElement('div');
        controls.className = 'agent-controls';
        controls.appendChild(presentationButton(agent));
        controls.appendChild(agentButton(agent, 'pause', 'Pause'));
        controls.appendChild(agentButton(agent, 'resume', 'Resume'));
        controls.appendChild(agentButton(agent, 'steer', 'Steer'));
        controls.appendChild(agentButton(agent, 'model', 'Model'));
        controls.appendChild(agentButton(agent, 'budget', 'Budget'));
        controls.appendChild(agentButton(agent, 'retry', 'Retry'));
        controls.appendChild(agentButton(agent, 'cancel', 'Cancel'));
        item.appendChild(controls);
      }
      list.appendChild(item);
    }
    restoreFocus(list, focus);
  }

  // ----------------------------------------------------------- transcript

  function evidenceIdOf(artifact) {
    if (typeof artifact !== 'string') {
      return null;
    }
    var match = /^(?:evidence:)?(\d+)$/.exec(artifact.trim());
    return match ? Number(match[1]) : null;
  }

  /**
   * The PURE scroll-ownership decision of one rebuild, computed BEFORE the
   * DOM mutates: the distance from the bottom and whether the view was
   * pinned (within the slack). Pinned-before stays pinned; otherwise the
   * first visible entry is preserved as the anchor.
   */
  function transcriptScrollPlan(input) {
    var distance = input.scrollHeight - input.scrollTop - input.clientHeight;
    return {
      distanceFromBottom: distance,
      pinned: distance <= TRANSCRIPT_PIN_SLACK_PX,
    };
  }

  /** Content identity of one rendered entry (host-mirroring). */
  function transcriptEntryKey(entry) {
    var tools = entry.tools || [];
    var out =
      entry.seq +
      ':' +
      entry.role +
      ':' +
      hash128(entry.text) +
      ':' +
      hash128(entry.reasoning) +
      ':' +
      hash128(entry.summary) +
      ':';
    for (var t = 0; t < tools.length; t++) {
      out +=
        tools[t].state +
        ':' +
        hash128(tools[t].name) +
        ':' +
        hash128(tools[t].excerpt) +
        ':' +
        hash128(tools[t].artifact) +
        ':' +
        (tools[t].exitCode === null || tools[t].exitCode === undefined
          ? 'n'
          : tools[t].exitCode) +
        ';';
    }
    return out;
  }

  /** Per-entry content signatures; joined for the cheap no-op compare. */
  function transcriptKeysOf(entries) {
    if (!entries || entries.length === 0) {
      return [];
    }
    var keys = [];
    for (var i = 0; i < entries.length; i++) {
      keys.push(transcriptEntryKey(entries[i]));
    }
    return keys;
  }

  /** Cheap signature of the rendered entries (host-mirroring). */
  function transcriptKeyOf(entries) {
    var keys = transcriptKeysOf(entries);
    return keys.length === 0 ? 'empty' : keys.join('|');
  }

  /** Capture open details / delivered evidence / focus of one entry node. */
  function captureEntryState(node) {
    var state = { open: [], evidence: [], focus: null };
    if (!node || !node.querySelectorAll) {
      return state;
    }
    var details = node.querySelectorAll('details');
    for (var i = 0; i < details.length; i++) {
      state.open.push(details[i].open === true);
    }
    var holders = node.querySelectorAll('[data-evidence]');
    var occurrence = {};
    for (var h = 0; h < holders.length; h++) {
      var evidenceId = holders[h].getAttribute('data-evidence');
      var pre = holders[h].querySelector ? holders[h].querySelector('pre') : null;
      if (pre) {
        occurrence[evidenceId] = occurrence[evidenceId] === undefined ? 0 : occurrence[evidenceId] + 1;
        state.evidence.push({
          id: evidenceId,
          occurrence: occurrence[evidenceId],
          node: pre.cloneNode(true),
        });
      }
    }
    var active = typeof document !== 'undefined' ? document.activeElement : null;
    if (active && node.contains && node.contains(active)) {
      state.focus = {
        path: elementPath(node, active),
        tag: active.tagName,
        text: active.textContent,
        aria: active.getAttribute ? active.getAttribute('aria-label') : null,
      };
    }
    return state;
  }

  /** Restore open details / delivered evidence / focus onto a fresh node. */
  function restoreEntryState(node, state) {
    if (!node || !state || !node.querySelectorAll) {
      return;
    }
    var details = node.querySelectorAll('details');
    for (var i = 0; i < details.length && i < state.open.length; i++) {
      details[i].open = state.open[i];
    }
    for (var e = 0; e < state.evidence.length; e++) {
      var recovered = state.evidence[e];
      // Pair by OCCURRENCE: two tool refs to the same artifact must each get
      // their own delivered text back, never both onto the first holder.
      var holders = node.querySelectorAll('[data-evidence="' + recovered.id + '"]');
      var holder = holders[recovered.occurrence];
      if (holder) {
        clear(holder);
        holder.appendChild(recovered.node);
      }
    }
    if (state.focus) {
      if (state.focus.path) {
        var exact = elementAtPath(node, state.focus.path);
        if (exact && exact.tagName === state.focus.tag && typeof exact.focus === 'function') {
          exact.focus();
          return;
        }
      }
      var candidates = node.querySelectorAll('button, input, textarea, select, a, [tabindex]');
      for (var c = 0; c < candidates.length; c++) {
        var candidate = candidates[c];
        var match =
          state.focus.aria !== null && state.focus.aria !== undefined
            ? candidate.getAttribute && candidate.getAttribute('aria-label') === state.focus.aria
            : candidate.textContent === state.focus.text;
        if (match && candidate.tagName === state.focus.tag) {
          if (typeof candidate.focus === 'function') {
            candidate.focus();
          }
          return;
        }
      }
    }
  }

  /** The first visible entry (index + pixel offset) of the CURRENT list. */
  function transcriptAnchorOf(container) {
    var offset = container.scrollTop;
    var children = container.children || [];
    for (var i = 0; i < children.length; i++) {
      var node = children[i];
      var top = typeof node.offsetTop === 'number' ? node.offsetTop : 0;
      var height = typeof node.offsetHeight === 'number' ? node.offsetHeight : 0;
      if (top + height > offset) {
        return { index: i, offset: offset - top };
      }
    }
    return { index: Math.max(0, children.length - 1), offset: 0 };
  }

  /** Restore the anchor pixel position of `anchor.index` after a rebuild. */
  function scrollToTranscriptAnchor(container, anchor) {
    var children = container.children || [];
    if (children.length === 0) {
      return;
    }
    var index = Math.min(Math.max(0, anchor.index), children.length - 1);
    var node = children[index];
    var top = typeof node.offsetTop === 'number' ? node.offsetTop : 0;
    container.scrollTop = top + anchor.offset;
  }

  function renderTranscript(entries) {
    var container = byId('entries');
    var key = transcriptKeyOf(entries);
    var keys = transcriptKeysOf(entries);
    var previousKey = transcriptKey;
    // Scroll intent is computed BEFORE any mutation: the streaming fast path
    // must honor the same pinned-follows / unpinned-anchor contract as a
    // rebuild, or a pinned reader silently stops following the stream.
    var plan = transcriptScrollPlan({
      scrollHeight: container.scrollHeight,
      scrollTop: container.scrollTop,
      clientHeight: container.clientHeight,
    });
    var pin = transcriptKey === null || plan.pinned || transcriptPinRequested;
    var anchor = pin ? null : transcriptAnchorOf(container);
    // Streaming fast path: every entry EXCEPT the last is byte-identical and
    // the last entry changed (a text/reasoning/tool delta). Replacing only
    // the last node preserves open details, delivered evidence, focus and
    // the reading position; a full rebuild here re-announced the whole
    // transcript and destroyed all of it on every chunk.
    var lastOnlyChanged =
      transcriptEntryKeys !== null &&
      transcriptEntryKeys.length === keys.length &&
      keys.length > 0 &&
      container.children.length === keys.length &&
      keys[keys.length - 1] !== transcriptEntryKeys[transcriptEntryKeys.length - 1] &&
      (function () {
        for (var p = 0; p < keys.length - 1; p++) {
          if (keys[p] !== transcriptEntryKeys[p]) {
            return false;
          }
        }
        return true;
      })();
    if (lastOnlyChanged) {
      var oldNode = container.children[container.children.length - 1];
      var preserved = captureEntryState(oldNode);
      var freshNode = renderEntry(entries[entries.length - 1]);
      // Attach FIRST: focus() on a detached node is a real-browser no-op, so
      // restoring before replaceChild lost keyboard focus on every delta.
      container.replaceChild(freshNode, oldNode);
      restoreEntryState(freshNode, preserved);
      transcriptKey = key;
      transcriptEntryKeys = keys;
      transcriptPinRequested = false;
      if (pin) {
        container.scrollTop = container.scrollHeight;
      } else {
        scrollToTranscriptAnchor(container, anchor);
      }
      return;
    }
    if (transcriptKey !== null && key === transcriptKey) {
      // No transcript delta: no rebuild and ZERO scroll mutation, so an
      // expanded evidence block and the reading position both survive a
      // background snapshot.
      return;
    }
    // A strict prefix extension (the common snapshot append) adds ONLY the
    // new entries: existing DOM nodes are never re-inserted, so the live
    // region announces additions instead of re-announcing the history.
    var appended =
      previousKey !== null &&
      previousKey !== 'empty' &&
      key.length > previousKey.length &&
      key.indexOf(previousKey) === 0 &&
      container.children.length > 0 &&
      entries.length >= container.children.length;
    if (appended) {
      for (var a = container.children.length; a < entries.length; a++) {
        container.appendChild(renderEntry(entries[a]));
      }
    } else {
      clear(container);
      if (!entries || entries.length === 0) {
        transcriptKey = key;
        transcriptEntryKeys = keys;
        transcriptPinRequested = false;
        // The empty slot stays empty: the onboarding/welcome state right
        // above the log explains the panel; a duplicated "no messages" line
        // would only restate it.
        if (pin) {
          container.scrollTop = 0;
        }
        return;
      }
      for (var i = 0; i < entries.length; i++) {
        container.appendChild(renderEntry(entries[i]));
      }
    }
    transcriptKey = key;
    transcriptEntryKeys = keys;
    transcriptPinRequested = false;
    if (pin) {
      container.scrollTop = container.scrollHeight;
    } else {
      scrollToTranscriptAnchor(container, anchor);
    }
  }

  // ------------------------------------------------------- message grammar
  // One compact activity grammar inside assistant turns: tool use reads as a
  // single row (icon, name, outcome chip), test/check/verify runs carry
  // pass/fail chips, file edits carry +/- counts when the daemon excerpt is
  // a diff, and commands carry their exit code. The excerpt and the evidence
  // affordance stay behind the row's own disclosure. Prose stays prose; no
  // verdict is ever fabricated beyond what the tool data carries.

  function classifyTool(name) {
    var value = String(name == null ? '' : name).toLowerCase();
    if (value.indexOf('test') !== -1) {
      return 'test';
    }
    if (
      value.indexOf('edit') !== -1 ||
      value.indexOf('write') !== -1 ||
      value.indexOf('patch') !== -1 ||
      value.indexOf('apply') !== -1 ||
      value.indexOf('create') !== -1 ||
      value.indexOf('replace') !== -1
    ) {
      return 'edit';
    }
    if (value.indexOf('verify') !== -1) {
      return 'verify';
    }
    if (
      value.indexOf('check') !== -1 ||
      value.indexOf('lint') !== -1 ||
      value.indexOf('diagno') !== -1 ||
      value.indexOf('compile') !== -1
    ) {
      return 'check';
    }
    if (
      value.indexOf('search') !== -1 ||
      value.indexOf('grep') !== -1 ||
      value.indexOf('glob') !== -1 ||
      value.indexOf('find') !== -1
    ) {
      return 'search';
    }
    if (
      value.indexOf('bash') !== -1 ||
      value.indexOf('shell') !== -1 ||
      value.indexOf('terminal') !== -1 ||
      value.indexOf('command') !== -1 ||
      value.indexOf('exec') !== -1
    ) {
      return 'command';
    }
    if (value.indexOf('read') !== -1 || value.indexOf('open') !== -1) {
      return 'read';
    }
    return 'tool';
  }

  var TOOL_GLYPHS = {
    command: [
      { tag: 'rect', attrs: { x: '2', y: '3.5', width: '12', height: '9', rx: '1.5' } },
      { tag: 'polyline', attrs: { points: '5,7 7.5,9 5,11' } },
      { tag: 'line', attrs: { x1: '8.5', y1: '11', x2: '11.5', y2: '11' } },
    ],
    edit: [
      { tag: 'path', attrs: { d: 'M3 13l1-4 8-8 3 3-8 8-4 1z' } },
      { tag: 'line', attrs: { x1: '10.5', y1: '3.5', x2: '12.5', y2: '5.5' } },
    ],
    test: [
      { tag: 'circle', attrs: { cx: '8', cy: '8', r: '5.5' } },
      { tag: 'polyline', attrs: { points: '5.5,8.2 7.2,10 10.5,6' } },
    ],
    verify: [
      { tag: 'path', attrs: { d: 'M8 1.5l5.5 2.3v4.2c0 3.5-2.5 5.6-5.5 6.5-3-.9-5.5-3-5.5-6.5V3.8z' } },
      { tag: 'polyline', attrs: { points: '5.5,8 7.2,9.8 10.5,6' } },
    ],
    check: [
      { tag: 'circle', attrs: { cx: '8', cy: '8', r: '5.5' } },
      { tag: 'line', attrs: { x1: '8', y1: '5.2', x2: '8', y2: '8.8' } },
      { tag: 'line', attrs: { x1: '8', y1: '10.9', x2: '8', y2: '11.1' } },
    ],
    search: [
      { tag: 'circle', attrs: { cx: '7', cy: '7', r: '4.5' } },
      { tag: 'line', attrs: { x1: '10.5', y1: '10.5', x2: '14', y2: '14' } },
    ],
    read: [
      { tag: 'path', attrs: { d: 'M3.5 2h6l3 3v9h-9z' } },
      { tag: 'line', attrs: { x1: '6', y1: '7', x2: '10.5', y2: '7' } },
      { tag: 'line', attrs: { x1: '6', y1: '9.5', x2: '10.5', y2: '9.5' } },
    ],
    tool: [
      { tag: 'polygon', attrs: { points: '8,2 9.6,6.4 14,8 9.6,9.6 8,14 6.4,9.6 2,8 6.4,6.4' } },
    ],
  };

  function toolGlyph(kind) {
    var svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('width', '14');
    svg.setAttribute('height', '14');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('class', 'activity-icon');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '1.3');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    var shapes = TOOL_GLYPHS[kind] || TOOL_GLYPHS.tool;
    for (var i = 0; i < shapes.length; i++) {
      var shape = shapes[i];
      var node = document.createElementNS('http://www.w3.org/2000/svg', shape.tag);
      for (var key in shape.attrs) {
        if (Object.prototype.hasOwnProperty.call(shape.attrs, key)) {
          node.setAttribute(key, shape.attrs[key]);
        }
      }
      svg.appendChild(node);
    }
    return svg;
  }

  var DIFF_SCAN_MAX_LINES = 400;

  /**
   * Honest +/- counts: only when the daemon excerpt carries diff markers
   * (git-style +/- lines). No markers, no counts — never a fabricated edit
   * statistic. The scan is bounded.
   */
  function diffCounts(excerpt) {
    if (typeof excerpt !== 'string' || excerpt.length === 0) {
      return null;
    }
    var lines = excerpt.split('\n');
    var limit = Math.min(lines.length, DIFF_SCAN_MAX_LINES);
    var adds = 0;
    var dels = 0;
    for (var i = 0; i < limit; i++) {
      var text = lines[i];
      if (text.length === 0) {
        continue;
      }
      if (text.charCodeAt(0) === 43) {
        if (text.indexOf('+++') !== 0) {
          adds += 1;
        }
      } else if (text.charCodeAt(0) === 45) {
        if (text.indexOf('---') !== 0) {
          dels += 1;
        }
      }
    }
    if (adds === 0 && dels === 0) {
      return null;
    }
    return { adds: adds, dels: dels };
  }

  /** The outcome tone of one tool from its exit code and durable state. */
  function toolOutcome(tool) {
    var exit = tool.exitCode;
    if (typeof exit === 'number' && isFinite(exit)) {
      return exit === 0 ? { tone: 'pass', label: 'exit 0' } : { tone: 'fail', label: 'exit ' + exit };
    }
    var state = String(tool.state == null ? '' : tool.state).trim().toLowerCase();
    var label = state.length > 0 ? state.replace(/_/g, ' ') : 'unknown';
    if (state === 'failed' || state === 'error' || state === 'errored') {
      return { tone: 'fail', label: label };
    }
    if (state === 'running' || state === 'in_progress' || state === 'pending' || state === 'validating') {
      return { tone: 'run', label: label };
    }
    if (state === 'completed' || state === 'done') {
      return { tone: 'pass', label: 'done' };
    }
    return { tone: 'neutral', label: label };
  }

  function activityChipOf(kind, outcome, counts) {
    if (kind === 'edit' && counts !== null) {
      return { tone: 'diff', label: '' };
    }
    if (kind === 'test' || kind === 'verify' || kind === 'check') {
      if (outcome.tone === 'pass') {
        return { tone: 'pass', label: 'passed' };
      }
      if (outcome.tone === 'fail') {
        return { tone: 'fail', label: 'failed' };
      }
      return { tone: outcome.tone, label: outcome.label };
    }
    if (kind === 'search' || kind === 'read' || kind === 'tool') {
      return { tone: outcome.tone === 'pass' ? 'neutral' : outcome.tone, label: outcome.label };
    }
    return { tone: outcome.tone, label: outcome.label };
  }

  function renderTool(container, tool) {
    var name = String(tool.name == null ? '' : tool.name);
    var kind = classifyTool(name);
    var outcome = toolOutcome(tool);
    var counts = kind === 'edit' ? diffCounts(tool.excerpt) : null;
    var chip = activityChipOf(kind, outcome, counts);

    var row = document.createElement('div');
    row.className = 'activity activity-' + kind + ' tone-' + outcome.tone;

    var details = document.createElement('details');
    details.className = 'activity-details';
    // A running or failed row opens itself: the live/broken output is the
    // part worth showing; settled rows stay compact until asked.
    if (outcome.tone === 'fail' || outcome.tone === 'run') {
      details.open = true;
    }
    var summary = document.createElement('summary');
    summary.className = 'activity-summary';
    summary.appendChild(toolGlyph(kind));
    span(summary, name.length > 0 ? name : '(unnamed tool)', 'activity-name');
    if (chip.tone === 'diff') {
      var diff = span(summary, '', 'activity-chip activity-chip-diff');
      span(diff, '+' + counts.adds, 'activity-add');
      span(diff, '\u2212' + counts.dels, 'activity-del');
    } else {
      span(summary, chip.label, 'activity-chip activity-chip-' + chip.tone);
    }
    details.appendChild(summary);

    if (tool.excerpt) {
      var excerpt = document.createElement('pre');
      excerpt.className = 'excerpt';
      excerpt.textContent = tool.excerpt;
      details.appendChild(excerpt);
    }

    var evidenceId = evidenceIdOf(tool.artifact);
    if (evidenceId !== null) {
      var holder = document.createElement('div');
      holder.className = 'evidence';
      holder.setAttribute('data-evidence', String(evidenceId));
      var button = document.createElement('button');
      button.type = 'button';
      button.textContent = 'View evidence #' + evidenceId;
      button.addEventListener('click', function () {
        vscode.postMessage({ type: 'retrieveEvidence', evidenceId: evidenceId });
      });
      holder.appendChild(button);
      details.appendChild(holder);
    }
    row.appendChild(details);
    container.appendChild(row);
  }

  function renderEntry(entry) {
    var wrapper = document.createElement('article');
    wrapper.className = 'entry entry-' + (entry.role === 'user' ? 'user' : 'assistant');

    var head = document.createElement('div');
    head.className = 'entry-head';
    head.textContent = entry.role + ' #' + entry.seq;
    wrapper.appendChild(head);

    if (entry.text) {
      var text = document.createElement('div');
      text.className = 'entry-text';
      text.textContent = entry.text;
      wrapper.appendChild(text);
    }
    if (entry.reasoning) {
      var details = document.createElement('details');
      details.className = 'reasoning-details';
      var summary = document.createElement('summary');
      summary.textContent = 'Reasoning';
      details.appendChild(summary);
      var reasoning = document.createElement('pre');
      reasoning.className = 'reasoning';
      reasoning.textContent = entry.reasoning;
      details.appendChild(reasoning);
      wrapper.appendChild(details);
    }
    if (entry.summary) {
      var summaryLine = document.createElement('div');
      summaryLine.className = 'entry-summary';
      summaryLine.textContent = entry.summary;
      wrapper.appendChild(summaryLine);
    }
    var tools = Array.isArray(entry.tools) ? entry.tools : [];
    for (var i = 0; i < tools.length; i++) {
      renderTool(wrapper, tools[i]);
    }
    return wrapper;
  }

  function setDaemon(status, detail) {
    var dot = byId('daemon-dot');
    if (dot) {
      dot.className = 'dot dot-' + (status || 'stopped');
    }
    // The service URL/detail is diagnostics: it renders only inside the
    // Diagnostics card, never in the product header.
    setText('daemon-url', detail || null);
  }

  /**
   * The product status line of the header: `Ready · <model>` while idle,
   * `Running · <n> agents · <state>` while a run is active. Service
   * start/stop vocabulary stays in Diagnostics.
   */
  function headerStatusText(snapshot) {
    var daemon = snapshot && snapshot.daemon ? String(snapshot.daemon) : 'stopped';
    if (daemon === 'starting') {
      return 'Starting service…';
    }
    if (daemon === 'error') {
      return 'Service unavailable';
    }
    if (daemon !== 'running') {
      return 'Ready';
    }
    var task = snapshot.task;
    var hasActiveRun = snapshot.busy === true || snapshot.activeRunId != null;
    if (hasActiveRun && task) {
      var state =
        typeof task.phase === 'string' && task.phase.length > 0
          ? task.phase
          : String(task.state == null ? '' : task.state);
      var agents = Array.isArray(snapshot.agents) ? snapshot.agents.length : 0;
      var parts = ['Running'];
      if (agents > 0) {
        parts.push(agents + (agents === 1 ? ' agent' : ' agents'));
      }
      if (state.length > 0) {
        parts.push(state);
      }
      return parts.join(' · ');
    }
    var model = snapshot.session && snapshot.session.model ? String(snapshot.session.model) : '';
    return model.length > 0 ? 'Ready · ' + model : 'Ready';
  }

  // ---------------------------------------------------------- attachments

  function formatBytes(value) {
    if (typeof value !== 'number' || !isFinite(value) || value < 0) {
      return '?';
    }
    if (value < 1024) {
      return String(value) + ' B';
    }
    if (value < 1024 * 1024) {
      return (value / 1024).toFixed(1) + ' KiB';
    }
    return (value / (1024 * 1024)).toFixed(1) + ' MiB';
  }

  function sanitizeAttachment(raw) {
    if (!raw || typeof raw !== 'object') {
      return null;
    }
    if (typeof raw.id !== 'string' || raw.id.length === 0 || raw.id.length > 128) {
      return null;
    }
    var bytes =
      typeof raw.bytes === 'number' && isFinite(raw.bytes) && raw.bytes >= 0 ? raw.bytes : 0;
    return {
      id: raw.id,
      filename: typeof raw.filename === 'string' ? raw.filename.slice(0, 255) : null,
      mime: typeof raw.mime === 'string' ? raw.mime.slice(0, 128) : '',
      bytes: bytes,
      isImage: raw.isImage === true,
      refusal: typeof raw.refusal === 'string' && raw.refusal.length > 0 ? raw.refusal : null,
    };
  }

  function setAttachments(items) {
    var next = [];
    if (Array.isArray(items)) {
      for (var i = 0; i < items.length && next.length < WEBVIEW_MAX_ATTACHMENTS; i++) {
        var item = sanitizeAttachment(items[i]);
        if (item) {
          next.push(item);
        }
      }
    }
    attachments = next;
    renderAttachments();
  }

  function renderAttachments() {
    var list = byId('attachment-list');
    var clearButton = byId('btn-clear-attachments');
    var notice = byId('attachment-notice');
    if (!list || !clearButton) {
      return;
    }
    var focus = captureFocus(list);
    clear(list);
    var firstRefusal = null;
    for (var i = 0; i < attachments.length; i++) {
      (function (item) {
        var row = document.createElement('li');
        row.className = 'attachment' + (item.refusal ? ' attachment-refused' : '');
        row.setAttribute('data-attachment-id', item.id);
        var label = document.createElement('span');
        label.className = 'attachment-label';
        label.textContent =
          (item.filename || '(unnamed)') + ' · ' + item.mime + ' · ' + formatBytes(item.bytes);
        row.appendChild(label);
        var status = document.createElement('span');
        status.className = 'attachment-status ' + (item.refusal ? 'status-refused' : 'status-ready');
        status.textContent = item.refusal ? 'refused' : 'ready';
        row.appendChild(status);
        if (item.refusal) {
          if (firstRefusal === null) {
            firstRefusal = item.refusal;
          }
          var refusal = document.createElement('span');
          refusal.className = 'attachment-refusal warn';
          refusal.textContent = item.refusal;
          row.appendChild(refusal);
        }
        var remove = document.createElement('button');
        remove.type = 'button';
        remove.className = 'attachment-remove';
        remove.textContent = 'Remove';
        remove.setAttribute('aria-label', 'Remove attachment ' + (item.filename || item.mime));
        remove.disabled = submitting;
        remove.addEventListener('click', function () {
          if (submitting) {
            return;
          }
          vscode.postMessage({ type: 'removeAttachment', id: item.id });
        });
        row.appendChild(remove);
        list.appendChild(row);
      })(attachments[i]);
    }
    clearButton.hidden = attachments.length === 0;
    clearButton.disabled = submitting;
    if (notice) {
      notice.hidden = firstRefusal === null;
      notice.textContent = firstRefusal === null ? '' : 'Attachment refused: ' + firstRefusal;
    }
    restoreFocus(list, focus);
  }

  /** Human wording for the stream status: never a raw `protocol_blocked`
   * tag in the meta row. */
  function streamStatusLabel(status) {
    var raw = status == null || status === '' ? 'stopped' : String(status);
    switch (raw) {
      case 'open':
        return 'live';
      case 'connecting':
        return 'connecting';
      case 'retrying':
        return 'reconnecting';
      case 'blocked':
      case 'protocol_blocked':
        return 'blocked (recovery available)';
      case 'stopped':
        return 'stopped';
      default:
        return raw.replace(/_/g, ' ');
    }
  }

  function base64FromBytes(bytes) {
    var out = '';
    for (var i = 0; i < bytes.length; i += 3) {
      var b0 = bytes[i];
      var b1 = i + 1 < bytes.length ? bytes[i + 1] : -1;
      var b2 = i + 2 < bytes.length ? bytes[i + 2] : -1;
      out += B64_ALPHABET.charAt(b0 >> 2);
      out += B64_ALPHABET.charAt(((b0 & 3) << 4) | (b1 >= 0 ? b1 >> 4 : 0));
      out += b1 >= 0 ? B64_ALPHABET.charAt(((b1 & 15) << 2) | (b2 >= 0 ? b2 >> 6 : 0)) : '=';
      out += b2 >= 0 ? B64_ALPHABET.charAt(b2 & 63) : '=';
    }
    return out;
  }

  function readAttachmentFile(file) {
    if (file && typeof file.arrayBuffer === 'function') {
      return file.arrayBuffer().then(function (buffer) {
        return new Uint8Array(buffer);
      });
    }
    return Promise.reject(new Error('file bytes are unavailable in this webview'));
  }

  function ingestFiles(fileList) {
    if (submitting || !fileList || fileList.length === 0) {
      return;
    }
    var room = WEBVIEW_MAX_ATTACHMENTS - attachments.length;
    if (room <= 0) {
      showNotice('error', 'attachment limit reached (' + WEBVIEW_MAX_ATTACHMENTS + ')');
      return;
    }
    var reads = [];
    var refused = [];
    for (var i = 0; i < fileList.length && reads.length < room; i++) {
      (function (file) {
        var name =
          typeof file.name === 'string' && file.name.length > 0 ? file.name.slice(0, 255) : null;
        var size = typeof file.size === 'number' ? file.size : -1;
        if (size < 0 || size > WEBVIEW_MAX_ATTACHMENT_BYTES) {
          refused.push(name || '(unnamed)');
          return;
        }
        var mime =
          typeof file.type === 'string' && file.type.length > 0
            ? file.type.slice(0, 128)
            : 'application/octet-stream';
        reads.push(
          readAttachmentFile(file).then(function (bytes) {
            if (bytes.byteLength > WEBVIEW_MAX_ATTACHMENT_BYTES) {
              refused.push(name || '(unnamed)');
              return null;
            }
            return {
              filename: name,
              mime: mime,
              bytes: bytes.byteLength,
              dataBase64: base64FromBytes(bytes),
            };
          }),
        );
      })(fileList[i]);
    }
    if (reads.length === 0) {
      showUserError(
        localUserError(
          'Some attachments were refused',
          'Adjust the files and try again; nothing was sent.',
          refused.join(', ') +
            ' exceeds the ' +
            formatBytes(WEBVIEW_MAX_ATTACHMENT_BYTES) +
            ' bound',
          { key: 'attachFiles', label: 'Choose files' },
        ),
      );
      return;
    }
    Promise.all(reads)
      .then(function (items) {
        var ready = [];
        for (var i = 0; i < items.length; i++) {
          if (items[i] !== null) {
            ready.push(items[i]);
          }
        }
        if (refused.length > 0) {
          showUserError(
            localUserError(
              'Some files were left out',
              'The others were attached; add the refused ones and try again if they matter.',
              refused.join(', '),
              { key: 'attachFiles', label: 'Choose files' },
            ),
          );
        }
        if (ready.length > 0) {
          vscode.postMessage({ type: 'attachData', items: ready });
        }
      })
      .catch(function (error) {
        showUserError(
          localUserError(
            'That attachment could not be read',
            'Try attaching it through the files picker instead.',
            error && error.message ? error.message : String(error),
            { key: 'attachFiles', label: 'Choose files' },
          ),
        );
      });
  }

  function renderStreamRecovery(status) {
    var section = byId('stream-recovery');
    if (!section) {
      return;
    }
    if (status === 'protocol_blocked') {
      section.hidden = false;
      setText('stream-recovery-reason', streamBlockedReason || 'the durable event stream is blocked');
      return;
    }
    section.hidden = true;
  }

  /**
   * The RAW resume position is diagnostics vocabulary: it renders only
   * inside the Diagnostics card, never in the recovery headline.
   */
  function renderStreamCursor() {
    var node = byId('stream-cursor');
    if (!node) {
      return;
    }
    if (streamBlockedReason === null) {
      node.hidden = true;
      node.textContent = '';
      return;
    }
    node.hidden = false;
    node.textContent =
      streamBlockedCursor === null
        ? 'Live updates blocked (the exact position was not reported).'
        : 'Live updates blocked at journal cursor ' + streamBlockedCursor + '.';
  }

  function dismissTransient() {
    var notice = byId('attachment-notice');
    if (notice) {
      notice.hidden = true;
    }
  }

  /**
   * The local Post-button policy: disabled while the draft violates the
   * shared byte bounds, with the exact refusal shown once a draft exists.
   */
  function updateBoardPostEnabled() {
    var subjectNode = byId('board-subject');
    var bodyNode = byId('board-body');
    var button = byId('btn-board-post');
    var notice = byId('board-draft-notice');
    if (!subjectNode || !bodyNode || !button || !notice) {
      return;
    }
    var reason = boardPolicy.boardDraftRefusal(subjectNode.value, bodyNode.value);
    var draftStarted = subjectNode.value.length > 0 || bodyNode.value.length > 0;
    button.disabled = reason !== null || boardPosting;
    if (reason !== null && draftStarted) {
      notice.hidden = false;
      // aria-atomic status: assigning the same string again re-announces it;
      // only replace the text node when the refusal actually changed.
      if (notice.textContent !== reason) {
        notice.textContent = reason;
      }
    } else if (!notice.hidden || notice.textContent !== '') {
      notice.hidden = true;
      notice.textContent = '';
    }
  }

  /**
   * The coordination board: truthful header, bounded post rows, and the
   * bounded composer. Every daemon string is rendered through textContent.
   */
  function renderBoard(board) {
    var header = byId('board-header');
    var list = byId('board-posts');
    if (!header || !list) {
      return;
    }
    header.textContent = boardPolicy.boardHeader(board);
    clear(list);
    var posts = boardPolicy.boardPostsForDisplay(board);
    if (board && board.available && posts.length === 0) {
      var empty = document.createElement('li');
      empty.className = 'muted';
      empty.textContent = 'no posts on this run-family board';
      list.appendChild(empty);
    }
    for (var i = 0; i < posts.length; i++) {
      var post = posts[i];
      var item = document.createElement('li');
      item.className = 'board-post';
      var head = document.createElement('div');
      head.className = 'board-post-head';
      head.textContent = '#' + (post.revision === null ? '?' : post.revision) + ' · ' + post.author;
      var subject = document.createElement('div');
      subject.className = 'board-post-subject';
      subject.textContent = post.subject;
      var body = document.createElement('div');
      body.className = 'board-post-body';
      body.textContent = post.body;
      item.appendChild(head);
      item.appendChild(subject);
      item.appendChild(body);
      list.appendChild(item);
    }
    updateBoardPostEnabled();
  }

  function renderSnapshot(snapshot) {
    if (!snapshot) {
      return;
    }
    var sessionId = snapshot.session ? String(snapshot.session.id) : null;
    if (currentSessionId !== null && sessionId !== currentSessionId) {
      // The host clears its store on a session switch (uploads are bound to
      // the session); the panel drops the same visible set and the pending
      // submission identity so a stale id can never leak into a new session.
      attachments = [];
      renderAttachments();
    }
    currentSessionId = sessionId;
    setDaemon(snapshot.daemon, snapshot.daemonDetail);
    setText('daemon-text', headerStatusText(snapshot));
    setText('session-title', snapshot.session ? snapshot.session.title : 'none');
    setText('machine-label', snapshot.machineLabel || snapshot.machineState);
    setText('stream-status', streamStatusLabel(snapshot.streamStatus));
    renderComposerModel(snapshot.session);
    renderStreamRecovery(snapshot.streamStatus);
    renderTask(snapshot.task, snapshot.cockpit, snapshot.agents);
    renderPermissions(snapshot.permissions);
    renderCockpit(snapshot.cockpit, snapshot.cockpitSections);
    renderAgents(snapshot.agents);
    renderBoard(snapshot.board);
    renderWelcome(snapshot.transcript);
    renderTranscript(snapshot.transcript);
    // The raw last error lives in Diagnostics; the structured projection
    // reached the panel through `userError`. A last error with no posted
    // projection (e.g. a cockpit projection failure) still surfaces ONCE
    // through the same user-error renderer, with the raw text demoted to
    // the technical disclosure.
    var lastError =
      typeof snapshot.lastError === 'string' && snapshot.lastError.length > 0
        ? snapshot.lastError
        : null;
    if (lastError === null) {
      lastRenderedError = null;
    } else if (lastError !== lastRenderedError) {
      lastRenderedError = lastError;
      showUserError({
        kind: 'unexpected',
        summary: 'Faktor hit a problem',
        hint: 'Try the action again; open Diagnostics for the raw detail.',
        action: null,
        technical: lastError,
        code: null,
      });
    }
  }

  /** The composer's model chip: session identity, quiet and non-interactive. */
  function renderComposerModel(session) {
    var node = byId('composer-model');
    if (!node) {
      return;
    }
    var provider = session && session.provider ? String(session.provider) : '';
    var model = session && session.model ? String(session.model) : '';
    var text = model.length > 0 && provider.length > 0 ? provider + ' / ' + model : model || provider;
    node.hidden = text.length === 0;
    node.textContent = text;
    if (text.length > 0) {
      node.setAttribute('title', 'Session model: ' + text);
    } else if (node.removeAttribute) {
      node.removeAttribute('title');
    }
  }

  function showNotice(level, message) {
    var notices = byId('notices');
    var node = document.createElement('div');
    node.className = level === 'error' ? 'notice error' : 'notice';
    node.textContent = String(message);
    notices.appendChild(node);
    while (notices.childNodes.length > 4) {
      notices.removeChild(notices.firstChild);
    }
    setTimeout(function () {
      if (node.parentNode === notices) {
        notices.removeChild(node);
      }
    }, 8000);
  }

  /**
   * The ONE structured user-error card: human summary + what to do + an
   * action button when one exists, with the typed code and raw bounded text
   * demoted behind "Technical details". Identical technical tails never
   * stack, and the list is bounded. An actionable error is NOT auto-dismissed
   * (the operator must see the next step); only the identity list is capped.
   */
  function dispatchUserErrorAction(key) {
    if (key === 'startService') {
      vscode.postMessage({ type: 'startDaemon' });
    } else if (key === 'reconnectStream') {
      vscode.postMessage({ type: 'recoverStream' });
    } else if (key === 'refresh') {
      vscode.postMessage({ type: 'refresh' });
    } else if (key === 'attachFiles') {
      vscode.postMessage({ type: 'attachPick' });
    } else if (key === 'focusComposer') {
      var goalNode = byId('goal');
      if (goalNode && typeof goalNode.focus === 'function') {
        goalNode.focus();
      }
    }
  }

  function removeUserError(node, technical) {
    var index = shownUserErrors.indexOf(technical);
    if (index !== -1) {
      shownUserErrors.splice(index, 1);
    }
    if (node && node.parentNode) {
      node.parentNode.removeChild(node);
    }
  }

  function showUserError(error) {
    if (!error || typeof error !== 'object') {
      return;
    }
    var notices = byId('notices');
    if (!notices) {
      return;
    }
    var technical =
      typeof error.technical === 'string' && error.technical.length > 0
        ? error.technical
        : 'no further detail was provided';
    if (shownUserErrors.indexOf(technical) !== -1) {
      return;
    }
    var node = document.createElement('div');
    node.className = 'notice error user-error';
    node.setAttribute('data-error-kind', String(error.kind == null ? 'unexpected' : error.kind));
    line(
      node,
      typeof error.summary === 'string' && error.summary.length > 0
        ? error.summary
        : 'Faktor hit a problem',
      'user-error-summary',
    );
    if (typeof error.hint === 'string' && error.hint.length > 0) {
      line(node, error.hint, 'muted user-error-hint');
    }
    var action = error.action;
    if (
      action &&
      typeof action === 'object' &&
      typeof action.key === 'string' &&
      typeof action.label === 'string' &&
      action.label.length > 0
    ) {
      var actions = document.createElement('div');
      actions.className = 'composer-actions';
      var button = document.createElement('button');
      button.type = 'button';
      button.className = 'user-error-action';
      button.textContent = action.label;
      button.addEventListener('click', function () {
        dispatchUserErrorAction(action.key);
      });
      actions.appendChild(button);
      node.appendChild(actions);
    }
    var details = document.createElement('details');
    details.className = 'technical-disclosure';
    var summary = document.createElement('summary');
    summary.textContent = 'Technical details';
    details.appendChild(summary);
    var code = typeof error.code === 'string' && error.code.length > 0 ? error.code : null;
    var pre = document.createElement('pre');
    pre.className = 'excerpt user-error-technical';
    pre.textContent = code !== null ? '[' + code + '] ' + technical : technical;
    details.appendChild(pre);
    node.appendChild(details);
    node.setAttribute('data-error-technical', technical);
    notices.appendChild(node);
    shownUserErrors.push(technical);
    while (shownUserErrors.length > MAX_USER_ERRORS) {
      var technicalToDrop = shownUserErrors[0];
      var holders = notices.querySelectorAll('[data-error-technical]');
      var drop = null;
      for (var h = 0; h < holders.length; h++) {
        if (holders[h].getAttribute('data-error-technical') === technicalToDrop) {
          drop = holders[h];
          break;
        }
      }
      if (drop) {
        removeUserError(drop, technicalToDrop);
      } else {
        shownUserErrors.shift();
      }
    }
  }

  /**
   * A webview-local refusal (an oversized clipboard image, a failed byte
   * read) through the SAME rendering contract the host projections use; the
   * raw reason is demoted, never the headline.
   */
  function localUserError(summary, hint, technical, action) {
    return {
      kind: 'attachment_refused',
      summary: summary,
      hint: hint,
      action: action || null,
      technical: String(technical == null ? 'no further detail was provided' : technical).slice(0, 500),
      code: null,
    };
  }

  function deliverEvidence(id, text, truncated) {
    var holders = document.querySelectorAll('[data-evidence="' + id + '"]');
    for (var i = 0; i < holders.length; i++) {
      var holder = holders[i];
      clear(holder);
      var pre = document.createElement('pre');
      pre.className = 'excerpt';
      pre.textContent = truncated ? text + '\n… (truncated)' : text;
      pre.setAttribute('tabindex', '-1');
      holder.appendChild(pre);
      // Keyboard activation replaced the button; focus follows the content.
      if (typeof pre.focus === 'function') {
        pre.focus();
      }
    }
  }

  /**
   * The composer submitting lock. While a logical submission awaits its
   * explicit result, Run task / New task, every completion-contract control
   * and the whole attachment surface (attach, clear, per-item remove) is
   * disabled; the goal textarea stays editable, but text typed while pending
   * can never join the pending request. Every outcome (success, typed
   * refusal, transport failure) re-enables.
   */
  function setSubmitting(value) {
    submitting = value;
    var ids = [
      'btn-send',
      'btn-new-task',
      'contract-local',
      'contract-commit',
      'contract-push',
      'contract-pr',
      'btn-attach',
      'btn-clear-attachments',
    ];
    for (var i = 0; i < ids.length; i++) {
      var node = byId(ids[i]);
      if (node) {
        node.disabled = value;
      }
    }
    renderAttachments();
  }

  window.addEventListener('message', function (event) {
    var message = event.data || {};
    if (message.type === 'snapshot') {
      renderSnapshot(message.snapshot);
    } else if (message.type === 'evidence') {
      deliverEvidence(message.id, message.text, message.truncated);
    } else if (message.type === 'boardPosted' || message.type === 'boardRefused') {
      // Answered by the host for EVERY post outcome and correlated by the
      // submission token: a stale ack can neither release a newer post nor
      // clear a newer (possibly identical) draft. Text edited while the post
      // was in flight is never touched.
      var boardSubject = byId('board-subject');
      var boardBody = byId('board-body');
      if (submittedBoard !== null && message.token === submittedBoard.token) {
        if (
          message.type === 'boardPosted' &&
          boardSubject &&
          boardBody &&
          boardSubject.value === submittedBoard.subject &&
          boardBody.value === submittedBoard.body
        ) {
          boardSubject.value = '';
          boardBody.value = '';
        }
        boardPosting = false;
        submittedBoard = null;
        updateBoardPostEnabled();
      }
    } else if (message.type === 'startResult') {
      // The explicit result releases the submitting lock on EVERY outcome.
      setSubmitting(false);
      var ok = message.ok === true;
      var goalNode = byId('goal');
      if (goalNode) {
        goalNode.value = composerPolicy.afterStart(goalNode.value, message.goal, ok);
      }
      if (ok) {
        // Durable acceptance: the host cleared its store, the panel drops
        // the visible set and the submission identity, and focus returns to
        // the composer for the next task.
        attachments = [];
          renderAttachments();
        // The completion contract is per task start: a successful ack
        // resets the finish-level radio group; a failure keeps it for the
        // retry.
        clearCompletionControls();
        if (goalNode && typeof goalNode.focus === 'function') {
          goalNode.focus();
        }
      } else if (goalNode && typeof goalNode.focus === 'function') {
        // A refused/failed start keeps the draft; focus returns to the
        // composer where the retry happens (the now-disabled Run button had
        // dropped keyboard focus to the body).
        goalNode.focus();
      }
    } else if (message.type === 'attachments') {
      setAttachments(message.items);
    } else if (message.type === 'attachmentsCleared') {
      attachments = [];
      renderAttachments();
    } else if (message.type === 'streamBlocked') {
      streamBlockedReason =
        typeof message.reason === 'string' && message.reason.length > 0 ? message.reason : null;
      streamBlockedCursor =
        typeof message.cursor === 'number' && isFinite(message.cursor) ? message.cursor : null;
      if (streamBlockedReason !== null) {
        var recovery = byId('stream-recovery');
        if (recovery) {
          recovery.hidden = false;
        }
        setText('stream-recovery-reason', streamBlockedReason);
      }
      renderStreamCursor();
    } else if (message.type === 'userError') {
      showUserError(message.error);
    } else if (message.type === 'notice') {
      showNotice(message.level, message.message);
    }
  });

  // The ordered finish choice. The visible control is ONE radio group
  // (`Leave changes locally` → commit → push → pull request); later options
  // imply the earlier steps, so the three backend booleans map directly from
  // the SELECTED LEVEL. The ids the host tests pin stay attached to the
  // commit/push/PR levels; plain chat never carries a contract.
  var FINISH_LEVELS = {
    'contract-commit': { include_commit: true, include_push: false, include_pr: false },
    'contract-push': { include_commit: true, include_push: true, include_pr: false },
    'contract-pr': { include_commit: true, include_push: true, include_pr: true },
  };
  var FINISH_LEVEL_IDS = ['contract-pr', 'contract-push', 'contract-commit'];

  function completionContractFromControls() {
    for (var i = 0; i < FINISH_LEVEL_IDS.length; i++) {
      var node = byId(FINISH_LEVEL_IDS[i]);
      if (node && node.checked === true) {
        var level = FINISH_LEVELS[FINISH_LEVEL_IDS[i]];
        return {
          include_commit: level.include_commit,
          include_push: level.include_push,
          include_pr: level.include_pr,
        };
      }
    }
    return null;
  }

  function clearCompletionControls() {
    var ids = ['contract-local', 'contract-commit', 'contract-push', 'contract-pr'];
    for (var i = 0; i < ids.length; i++) {
      var node = byId(ids[i]);
      if (node) {
        node.checked = false;
      }
    }
    var local = byId('contract-local');
    if (local) {
      local.checked = true;
    }
  }

  /**
   * Start one logical submission. Single-flight: the flag gates the composer
   * BEFORE the post, so a double click / Enter+click race can never emit a
   * second sendGoal; it is released ONLY by an explicit host result. The
   * message is the IMMUTABLE snapshot: the attachment envelope carries the
   * host-side ids in visible order; the host owns the logical submission id
   * (minted from the complete parsed snapshot, never from a webview key).
   */
  function requestStart() {
    if (submitting) {
      return;
    }
    var goal = byId('goal').value.trim();
    if (!goal) {
      return;
    }
    // Draft preservation: the goal is NOT cleared here. The extension posts
    // a startResult; only a successful start clears the (unchanged) draft.
    // The Task-mode completion contract rides only THIS task start; plain
    // chat never carries it.
    var contract = completionContractFromControls();
    var message = { type: 'sendGoal', goal: goal };
    if (attachments.length > 0) {
      var ids = [];
      for (var i = 0; i < attachments.length; i++) {
        ids.push(attachments[i].id);
      }
      message.attachmentIds = ids;
    }
    if (contract) {
      message.completionContract = contract;
    }
    transcriptPinRequested = true;
    setSubmitting(true);
    vscode.postMessage(message);
  }

  byId('composer').addEventListener('submit', function (event) {
    event.preventDefault();
    requestStart();
  });

  // Board: an explicit top-of-board read acknowledges the page; the bounded
  // composer posts exactly the subject/body the host re-validates.
  byId('btn-board-read').addEventListener('click', function () {
    vscode.postMessage({ type: 'boardRead', since: null, limit: null });
  });
  byId('board-post').addEventListener('submit', function (event) {
    if (composing) {
      return;
    }
    if (event.preventDefault) {
      event.preventDefault();
    }
    var subjectNode = byId('board-subject');
    var bodyNode = byId('board-body');
    var reason = boardPolicy.boardDraftRefusal(subjectNode.value, bodyNode.value);
    if (reason !== null || boardPosting) {
      updateBoardPostEnabled();
      return;
    }
    boardPosting = true;
    boardSubmitSeq += 1;
    submittedBoard = {
      token: 'bp-' + boardSubmitSeq,
      subject: subjectNode.value,
      body: bodyNode.value,
    };
    vscode.postMessage({
      type: 'boardPost',
      token: submittedBoard.token,
      subject: submittedBoard.subject,
      body: submittedBoard.body,
    });
    updateBoardPostEnabled();
  });
  byId('board-subject').addEventListener('input', updateBoardPostEnabled);
  byId('board-body').addEventListener('input', updateBoardPostEnabled);

  // Keyboard: Enter inserts a newline; Ctrl/Cmd+Enter starts the task.
  if (typeof document.addEventListener === 'function') {
    document.addEventListener('compositionstart', function () {
      composing = true;
    });
    document.addEventListener('compositionend', function () {
      composing = false;
    });
  }

  byId('goal').addEventListener('keydown', function (event) {
    if (
      composing ||
      event.isComposing === true ||
      event.keyCode === 229 ||
      event.key === 'Process'
    ) {
      // IME composition confirm: never start the task from a composing chord.
      return;
    }
    if (event.key === 'Enter' && (event.ctrlKey || event.metaKey)) {
      if (event.preventDefault) {
        event.preventDefault();
      }
      requestStart();
    }
  });

  // Paste image affordance: bounded bytes transit once; the host validates,
  // stores and later reports metadata only.
  byId('goal').addEventListener('paste', function (event) {
    var clipboard = event.clipboardData;
    if (!clipboard) {
      return;
    }
    var files = [];
    var items = clipboard.items || [];
    for (var i = 0; i < items.length; i++) {
      var item = items[i];
      if (item && item.kind === 'file' && item.type && item.type.indexOf('image/') === 0) {
        var file = typeof item.getAsFile === 'function' ? item.getAsFile() : null;
        if (file) {
          files.push(file);
        }
      }
    }
    if (files.length > 0) {
      if (event.preventDefault) {
        event.preventDefault();
      }
      ingestFiles(files);
    }
  });

  // Drag/drop target.
  byId('composer').addEventListener('dragover', function (event) {
    if (event.preventDefault) {
      event.preventDefault();
    }
  });
  byId('composer').addEventListener('drop', function (event) {
    if (event.preventDefault) {
      event.preventDefault();
    }
    var transfer = event.dataTransfer || {};
    ingestFiles(transfer.files);
  });

  // Escape dismisses the transient attachment-refusal notice.
  window.addEventListener('keydown', function (event) {
    if (event && event.key === 'Escape') {
      dismissTransient();
    }
  });

  byId('btn-attach').addEventListener('click', function () {
    if (submitting) {
      return;
    }
    // The HOST opens the native picker and reads the files: no file bytes
    // ever transit the webview for a picker selection.
    vscode.postMessage({ type: 'attachPick' });
  });
  byId('btn-clear-attachments').addEventListener('click', function () {
    if (submitting) {
      return;
    }
    vscode.postMessage({ type: 'clearAttachments' });
  });
  // Inspect is progressive disclosure (audit UI-1): the conversation, the
  // current run and the composer stay primary; run details, agents, board
  // and evidence live behind this one explicit toggle. The DOM stays intact
  // (and focusable) — only the visual grouping changes.
  byId('btn-inspect').addEventListener('click', function () {
    var app = byId('app');
    var collapsed = app.classList.toggle('inspect-collapsed');
    byId('btn-inspect').setAttribute('aria-expanded', collapsed ? 'false' : 'true');
  });
  // The `⋯` overflow holds the service/diagnostics vocabulary: session and
  // stream metadata, the raw service URL, Refresh/Start/Stop and the raw
  // stream position. Nothing in it is product copy, so it stays collapsed
  // until asked for.
  byId('btn-overflow').addEventListener('click', function () {
    var panel = byId('diagnostics');
    var button = byId('btn-overflow');
    var open = panel.hidden === true;
    panel.hidden = !open;
    button.setAttribute('aria-expanded', open ? 'true' : 'false');
  });
  byId('btn-refresh-snapshot').addEventListener('click', function () {
    vscode.postMessage({ type: 'refresh' });
  });
  byId('btn-reconnect-stream').addEventListener('click', function () {
    vscode.postMessage({ type: 'recoverStream' });
  });
  byId('btn-new-task').addEventListener('click', function () {
    if (submitting) {
      return;
    }
    vscode.postMessage({ type: 'newTask' });
    var goalNode = byId('goal');
    if (goalNode && typeof goalNode.focus === 'function') {
      goalNode.focus();
    }
  });
  byId('btn-cancel-run').addEventListener('click', function () {
    vscode.postMessage({ type: 'cancelRun' });
  });
  byId('btn-refresh').addEventListener('click', function () {
    vscode.postMessage({ type: 'refresh' });
  });
  byId('btn-start').addEventListener('click', function () {
    vscode.postMessage({ type: 'startDaemon' });
  });
  byId('btn-stop').addEventListener('click', function () {
    vscode.postMessage({ type: 'stopDaemon' });
  });

  // Onboarding prompt chips: fill the composer and focus it (feed, never
  // submit). They live in the empty-state markup, so the static markup pin
  // covers their labels and their data-prompt payloads.
  var welcomeChips =
    typeof document.querySelectorAll === 'function'
      ? document.querySelectorAll('.welcome-chip')
      : [];
  for (var welcomeIndex = 0; welcomeIndex < welcomeChips.length; welcomeIndex++) {
    (function (chip) {
      chip.addEventListener('click', function () {
        var prompt = chip.getAttribute ? chip.getAttribute('data-prompt') : null;
        if (!prompt) {
          return;
        }
        var goalNode = byId('goal');
        if (!goalNode) {
          return;
        }
        goalNode.value = prompt;
        if (typeof goalNode.focus === 'function') {
          goalNode.focus();
        }
      });
    })(welcomeChips[welcomeIndex]);
  }

  vscode.postMessage({ type: 'ready' });
})();
