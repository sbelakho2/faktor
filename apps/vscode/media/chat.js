// Faktor chat webview script. Hand-written, dependency-free, no remote
// loads, no eval: it renders the snapshot the extension posts and sends
// typed commands back. All daemon-derived strings go through textContent.
// Composer draft policy comes from media/composer-state.js (loaded first).
(function () {
  'use strict';

  var vscode = window.__faktorVsCode || (window.__faktorVsCode = acquireVsCodeApi());
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
  // History presentation policy comes from media/history-state.js (loaded
  // first); the fallback keeps the surface honest without it: nothing is
  // fabricated, the T31 copy is exact, and no row is ever rendered.
  var historyPolicy = (typeof FaktorHistory !== 'undefined' && FaktorHistory) || {
    HISTORY_UNAVAILABLE_LABEL: 'History unavailable',
    HISTORY_UNAVAILABLE_COPY: 'The conversation list could not be read.',
    HISTORY_UNAVAILABLE_ACTION: 'Refresh history',
    HISTORY_CAP_NOTE: 'the conversation list policy is unavailable',
    HISTORY_SEPARATION_NOTE: 'conversation state is not verification evidence',
    HISTORY_EMPTY: 'No conversations are served yet.',
    HISTORY_NO_MATCH: 'No conversation title matches this search.',
    MAX_HISTORY_QUERY: 200,
    sessionDateLabel: function () {
      return null;
    },
    historyTitle: function () {
      return 'conversation';
    },
    historyStateLabel: function () {
      return 'unknown';
    },
    historyRows: function () {
      return { rows: [], total: 0, shown: 0, capped: false };
    },
    historyStatusText: function () {
      return 'history unavailable (policy missing)';
    },
  };
  // The T27/T28 recovery presentation (I06e). The mode is chosen from the
  // host's stream status + the explicit recovery message; the copy is frozen
  // and the actions are exactly Wait/Refresh and Reconnect/Refresh from
  // snapshot — a skip action is never offered.
  var STREAM_RECOVERY = {
    reconnecting: {
      label: 'Reconnecting live updates',
      copy: 'The last valid journal position is kept.',
      actions: [
        { key: 'wait', label: 'Wait' },
        { key: 'refresh', label: 'Refresh' },
      ],
    },
    paused: {
      label: 'Live updates paused',
      copy: 'The client cannot read the next durable event. Nothing is skipped.',
      actions: [
        { key: 'reconnectStream', label: 'Reconnect' },
        { key: 'refreshSnapshot', label: 'Refresh from snapshot' },
      ],
    },
  };

  // The approved composer/run state copy (T06/T07/T08/T09/T11/T12/T25/T26).
  // Every strip renders one glyph plus the exact mandated label and
  // supporting copy; the useful action stays the EXISTING control (Run task,
  // the approval card, Cancel run, the Inspect subviews, the kept draft and
  // New task) — no duplicate action buttons are invented here.
  var COMPOSER_STATES = {
    ready: {
      glyph: '\u2713',
      label: 'Ready',
      copy: 'Describe a task or ask a question.',
    },
    starting: {
      glyph: '\u2026',
      label: 'Starting the run',
      copy: 'Your captured draft is being admitted once.',
    },
    startUnavailable: {
      glyph: '!',
      label: "Couldn't start",
      copy: 'The draft was kept. Retry uses the same request.',
    },
    needsInput: {
      glyph: '!',
      label: 'Needs your input',
      copy: 'The run is waiting for a decision or clarification.',
    },
  };
  var RUN_STATES = {
    running: {
      glyph: '\u25b6',
      label: 'Running',
      copy: 'The candidate is separate from your workspace.',
    },
    checking: {
      glyph: '\u2026',
      label: 'Checking the change',
      copy: 'Required results remain attached to this candidate.',
    },
    paused: {
      glyph: '\u23f8',
      label: 'Paused',
      copy: 'Your task is saved. Resume it when you are ready; nothing needs to be sent again.',
    },
    cancelled: {
      glyph: '\u2298',
      label: 'Run cancelled',
      copy: 'Cancellation is a terminal run outcome, not verification success.',
    },
    failed: {
      glyph: '\u2717',
      label: 'Run failed',
      copy: 'The result and reason remain available for inspection.',
    },
  };
  var COMPOSER_STATE_IDS = {
    container: 'composer-state',
    glyph: 'composer-state-glyph',
    label: 'composer-state-label',
    copy: 'composer-state-copy',
  };
  // One logical submission at a time. The flag gates the composer BEFORE the
  // post, so a double click / Enter+click race can never emit a second
  // sendGoal; it is released ONLY by an explicit host result.
  var submitting = false;
  /** The ONE composer's explicit intent. Ask is the product default. */
  var composerIntent = 'ask';
  /**
   * True after the host explicitly reported a RETRYABLE failed start
   * (`startResult` ok false with `retryable: true`): the draft was kept and
   * the retry checks the same logical submission. A typed refusal keeps the
   * draft but never sets this row. Cleared by the next submission and by a
   * durable start.
   */
  var startFailed = false;
  /** The last served session agent state (`needs_user_input` = T11). */
  var machineStateValue = 'unknown';
  /** True while a non-terminal run is tracked (T06 stays for the idle panel). */
  var runActive = false;
  /** True while a board post awaits its durable ack (double-click guard). */
  var boardPosting = false;
  /** The exact draft a board post submitted (token-correlated ack). */
  var submittedBoard = null;
  var boardSubmitSeq = 0;
  /**
   * The per-request approval in-flight set: a clicked request's Allow/Deny
   * stay disabled until the host reports THAT request's result (never a
   * global lock). Refusal reasons stay on the card while the request is
   * still visible; both are bounded.
   */
  // Insertion-ordered Set: membership is the per-button gate. The historical
  // array made every rendered card scan up to MAX_PENDING_PERMISSIONS ids.
  var permissionInFlight = new Set();
  var permissionRefusals = [];
  var MAX_PERMISSION_REFUSALS = 8;
  var MAX_PERMISSION_REASON_CHARS = 240;
  /** The last rendered pending set (refusal re-render source). */
  var lastPermissions = [];
  /**
   * The ids of the last rendered pending set: a NEW approval is announced
   * exactly once (bounded aria-live), and an unchanged re-render never
   * re-announces or steals focus.
   */
  var lastPermissionIds = null;
  /** True while an IME composition is active (chord/Enter must not submit). */
  var composing = false;
  // Scroll-ownership state: the content keys of the rendered transcript and
  // the one-shot pin requested by the user's own submission. Long transcripts
  // are windowed (only the visible slice plus overscan is materialized) so DOM
  // nodes stay bounded; heights are cached per entry and the top/bottom
  // spacers reproduce the full scroll range.
  var transcriptKey = null;
  var transcriptEntryKeys = null;
  var transcriptEntryStructures = null;
  var transcriptEntries = null;
  var transcriptRevision = null;
  var transcriptPinRequested = false;
  var transcriptWindowed = false;
  var transcriptWindowStart = 0;
  var transcriptWindowEnd = 0;
  var transcriptHeights = [];
  var transcriptPrefix = [0];
  var transcriptScrollScheduled = false;
  var transcriptScrollListenerAttached = false;
  var transcriptSuppressScroll = false;
  // Explicit scroll-ownership state machine. Pinned is only changed by an
  // EXTERNAL scroll event (user intent) or an explicit own-submission pin;
  // the programmatic writes made by this renderer are recognized by their
  // exact position and can never flip the state themselves.
  var transcriptPinned = true;
  var transcriptAnchor = null;
  var transcriptLastWrittenTop = 0;
  // Only a real input gesture (wheel, touch, pointer/scrollbar, keyboard
  // scroll keys) may change the pin. Browser-internal scroll adjustments and
  // this renderer's own follow writes therefore never fight the user.
  var transcriptUserScrollUntil = 0;
  var TRANSCRIPT_PIN_SLACK_PX = 24;
  // Beyond this entry count only the visible slice is materialized. The cap
  // below it keeps the common short conversation exactly as it rendered
  // before (no spacers, no window bookkeeping).
  var TRANSCRIPT_WINDOW_THRESHOLD = 120;
  var TRANSCRIPT_WINDOW_OVERSCAN = 12;
  var TRANSCRIPT_WINDOW_MIN_ROWS = 8;
  var TRANSCRIPT_ESTIMATED_ENTRY_PX = 64;
  var TRANSCRIPT_ENTRY_GAP_PX = 12;
  // Host-side attachment metadata (id + filename + mime + size only: the
  // host owns the bytes). Paste/drop bytes transit once through the webview,
  // bounded; a picker selection never ships base64 through here.
  var attachments = [];
  var currentSessionId = null;
  var streamBlockedReason = null;
  var streamBlockedCursor = null;
  /** The last emitted stream status (snapshot source for the T27/T28 card). */
  var streamStatusValue = 'stopped';
  /** True while an explicit reconnect attempt is in flight (T27). */
  var streamRecovering = false;
  /** The operator chose Wait: hide T27 until the next recovery/paused event. */
  var recoveryWaitDismissed = false;
  /** The last rendered preflight view (diagnostics mirror source). */
  var lastPreflight = null;
  // Navigation shell state: the ONE active view (Work is the default and the
  // host never forces it back), the active Inspect subview, the last snapshot
  // (subview re-render source), the bounded history search text, the
  // in-flight row Open identity and the diagnostics popover state.
  var activeView = 'work';
  function safeSetTimeout(fn, ms) {
    try {
      if (typeof setTimeout === 'function') return setTimeout(fn, ms);
      if (typeof window !== 'undefined' && typeof window.setTimeout === 'function') return window.setTimeout(fn, ms);
    } catch (e) {}
    return 0;
  }
  var activeSubview = 'overview';
  var lastSnapshot = null;
  var historyQuery = '';
  var historyOpenInFlight = null;
  var overflowOpen = false;
  // The chat body has exactly ONE problem surface. Every failure source
  // (served attention, service authority, a blocked stream, a host user
  // error) writes its LATEST distinct problem here; the renderer chooses the
  // highest-priority active one and never stacks duplicate cards.
  var problemSources = { attention: null, authority: null, stream: null, user: null };
  var renderedProblemKey = null;
  var activeMenuAction = null;
  var VIEW_PANELS = {
    work: 'work-panel',
    settings: 'settings-panel',
    inspect: 'inspect-panel',
    history: 'history-panel',
    diagnostics: 'diagnostics',
  };
  // I03: the locally selected change (preserved by PATH across refreshes,
  // clamped when the path disappears) and the last explicit review result
  // for exactly that path. I07: the active History sub-tab and the render
  // cache so a conversation snapshot never rebuilds the outcomes surface.
  var selectedChangePath = null;
  var selectedChangeIndex = -1;
  var changeReviewState = null;
  var activeHistoryTab = 'conversations';
  var renderedOutcomes = null;
  var renderedOutcomesKey = null;
  /**
   * Content keys of the last rendered Changes / History lists. The host
   * re-posts the WHOLE snapshot for every stream delta (the store object is
   * structured-cloned across the webview boundary, so per-field identity
   * never survives); these keys let an unchanged section skip the DOM
   * rebuild entirely — an identical delta keeps focus, expansion state and
   * reading position instead of tearing 1000 rows down and re-creating them.
   * A section that renders its unavailable state resets its key so the
   * recovery snapshot always rebuilds.
   */
  var renderedChangesKey = null;
  var renderedHistoryKey = null;
  var renderedAgentsKey = null;
  var renderedBoardKey = null;
  var renderedCockpitKey = null;
  var renderedTaskKey = null;
  var renderedPermissionsKey = null;
  /** TRUE while an explicit outcomes read awaits its snapshot. */
  var outcomesReading = false;
  /** The last raw snapshot error rendered, so it is not re-announced. */
  var lastRenderedError = null;
  var MAX_PENDING_PERMISSIONS = 16;
  var WEBVIEW_MAX_ATTACHMENT_BYTES = 7 * 1024 * 1024;
  var WEBVIEW_MAX_ATTACHMENTS = 8;
  var B64_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

  function byId(id) {
    return document.getElementById(id);
  }

  /**
   * Instrumented render seam: the selftest installs
   * `window.__faktorRenderProbe` to count list REBUILDS (evidence that an
   * unchanged delta skipped the DOM work). With no probe installed this is
   * a no-op: the guard is the production behavior, the counter is evidence.
   */
  function renderProbe(kind) {
    try {
      var probe = typeof window !== 'undefined' ? window.__faktorRenderProbe : null;
      if (probe && typeof probe.count === 'function') {
        probe.count(kind);
      }
    } catch (error) {
      // A probe must never affect rendering.
    }
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

  /**
   * JSON text for content keys. Outcome/usage projections carry exact money
   * as BigInt in some hosts; plain JSON.stringify throws on those, so the
   * replacer renders them without losing the exact value.
   */
  function jsonForHash(value) {
    try {
      var text = JSON.stringify(value, function (key, entry) {
        return typeof entry === 'bigint' ? entry.toString() + 'n' : entry;
      });
      return text === undefined ? 'undefined' : text;
    } catch (error) {
      return String(value);
    }
  }

  /**
   * Bounded content cache for the pure derivations the transcript re-renders
   * per delta (diff line counts, parsed permission-detail reasons). The hash
   * is only the slot (bounded memory); the entry keeps the raw source and a
   * hit must match it EXACTLY, so a hash collision can never serve another
   * excerpt's counts/reason. The oldest slot is evicted past the cap.
   */
  var RENDER_CACHE_MAX = 512;
  var renderCaches = { diff: {}, reason: {} };
  var renderCacheOrder = { diff: [], reason: [] };

  function cachedRender(kind, key, source, compute) {
    var cache = renderCaches[kind];
    var existing = Object.prototype.hasOwnProperty.call(cache, key) ? cache[key] : null;
    if (existing && existing.source === source) {
      return existing.value;
    }
    var value = compute();
    cache[key] = { source: source, value: value };
    var order = renderCacheOrder[kind];
    if (!existing) {
      order.push(key);
    }
    if (order.length > RENDER_CACHE_MAX) {
      var drop = order.shift();
      delete cache[drop];
    }
    return value;
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
    var next = value === null || value === undefined || value === '' ? '—' : String(value);
    // Same-value writes still replace the text node (a DOM mutation that can
    // repaint the sticky header on every stream delta): skip them.
    if (node.textContent !== next) {
      node.textContent = next;
    }
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
    // Content guard: every stream delta re-posts the whole snapshot, but an
    // unchanged cockpit must not tear its controls, focus and disclosures
    // down on every delta.
    var guardKey =
      activeSubview +
      '#' +
      hash128(jsonForHash(view === undefined ? null : view)) +
      '#' +
      hash128(jsonForHash(Array.isArray(sections) ? sections : []));
    if (renderedCockpitKey !== null && renderedCockpitKey === guardKey) {
      return;
    }
    renderedCockpitKey = guardKey;
    renderProbe('cockpit');
    var focus = captureFocus(node);
    clear(node);
    if (!Array.isArray(sections) || sections.length === 0) {
      return;
    }
    for (var i = 0; i < sections.length; i++) {
      var section = sections[i];
      // The "Needs disclosure" block leads the run details: text-first (the
      // core's own state token plus the summary), never color-only, and it
      // is rendered as a distinct warning block rather than a quiet section.
      if (section.key === 'disclosure') {
        var warning = document.createElement('section');
        warning.className = 'cockpit-disclosure';
        warning.setAttribute('role', 'note');
        var warningHead = document.createElement('h3');
        warningHead.textContent = section.title;
        warning.appendChild(warningHead);
        var warningLines = Array.isArray(section.lines) ? section.lines : [];
        for (var w = 0; w < warningLines.length; w++) {
          line(warning, warningLines[w], 'disclosure-line');
        }
        node.appendChild(warning);
        continue;
      }
      var block = document.createElement('section');
      block.className = 'cockpit-section' + (section.present ? '' : ' cockpit-empty');
      var head = document.createElement('h3');
      head.textContent = section.title;
      block.appendChild(head);
      // The acceptance-criteria section renders one PROOF row per criterion
      // when the host serves the structured rows; older snapshots fall back
      // to the bounded lines (whose `[verdict]` prefix still styles).
      // Structured proof rows render whenever a section serves them; the
      // acceptance section replaces its text lines with the rows (today's
      // contract), while the Verification section keeps its status lines AND
      // gains the selectable criterion rows (`Open claim`).
      var criteria = Array.isArray(section.criteria) ? section.criteria : [];
      if (criteria.length > 0) {
        for (var c = 0; c < criteria.length; c++) {
          renderCriterionRow(block, criteria[c]);
        }
        if (section.key !== 'acceptance') {
          var extraLines = Array.isArray(section.lines) ? section.lines : [];
          for (var x = 0; x < extraLines.length; x++) {
            line(block, extraLines[x], 'muted');
          }
        }
      } else if (section.key === 'contract' && section.contract) {
        renderContractSection(block, section.contract, section.lines);
      } else {
        var lines = Array.isArray(section.lines) ? section.lines : [];
        var evidenceList = null;
        if (section.key === 'evidence' && lines.length > 0) {
          // The evidence region is a list; each ref is one listitem with an
          // accessible name (the served label).
          evidenceList = document.createElement('div');
          evidenceList.className = 'evidence-list';
          evidenceList.setAttribute('role', 'list');
          evidenceList.setAttribute(
            'aria-label',
            'Evidence: ' + String(section.title == null ? 'section' : section.title),
          );
          block.appendChild(evidenceList);
        }
        for (var j = 0; j < lines.length; j++) {
          if (section.key === 'evidence') {
            renderEvidenceLine(evidenceList || block, section, lines[j], j);
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
    row.setAttribute('role', 'listitem');
    row.setAttribute(
      'aria-label',
      typeof label === 'string' && label.length > 0 ? label : 'Evidence',
    );
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
    if (typeof value.criterionKey === 'string' && value.criterionKey.length > 0) {
      // The stable target of `Open claim` (select the related criterion).
      card.setAttribute('data-criterion-key', value.criterionKey);
    }
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
    if (verdict === 'unavailable') {
      // T15: a claim with no verdict says so explicitly; unavailable and
      // refused stay distinct and a positive verdict is never rendered here.
      line(card, 'No verdict is served for this claim.', 'warn criterion-no-verdict');
    }
    block.appendChild(card);
  }

  /**
   * The Contract section: constraints and invariants as statement rows,
   * risks with a severity chip and an explicit disclosure line for
   * high/unknown, and proof obligations with claim, evidence kind, a state
   * chip carrying confidence (`supported · 91%`) and provenance. Every row
   * carries its origin label; nothing renders color-only.
   */
  function renderContractSection(block, contract, noteLines) {
    function statementGroup(label, rows) {
      if (!Array.isArray(rows) || rows.length === 0) {
        return;
      }
      line(block, label, 'contract-group-label');
      for (var i = 0; i < rows.length; i++) {
        var row = document.createElement('div');
        row.className = 'contract-row';
        span(row, rows[i].statement, 'contract-statement');
        span(row, rows[i].origin, 'chip chip-origin');
        block.appendChild(row);
      }
    }
    statementGroup('Constraints', contract.constraints);
    statementGroup('Invariants', contract.invariants);
    var risks = Array.isArray(contract.risks) ? contract.risks : [];
    if (risks.length > 0) {
      line(block, 'Risks', 'contract-group-label');
      for (var r = 0; r < risks.length; r++) {
        var risk = risks[r];
        var riskRow = document.createElement('div');
        riskRow.className = 'contract-row';
        span(riskRow, risk.description, 'contract-statement');
        span(riskRow, risk.severity, 'chip chip-severity chip-severity-' + String(risk.severity));
        span(riskRow, risk.origin, 'chip chip-origin');
        block.appendChild(riskRow);
        if (risk.disclosure) {
          line(block, 'disclosure: ' + String(risk.disclosure), 'contract-disclosure-text');
        }
      }
    }
    var obligations = Array.isArray(contract.obligations) ? contract.obligations : [];
    if (obligations.length > 0) {
      line(block, 'Proof obligations', 'contract-group-label');
      for (var o = 0; o < obligations.length; o++) {
        var obligation = obligations[o];
        var obligationRow = document.createElement('div');
        obligationRow.className = 'contract-row';
        span(obligationRow, obligation.claim, 'contract-statement');
        span(obligationRow, obligation.evidence, 'contract-evidence');
        span(
          obligationRow,
          obligation.stateText,
          'chip chip-state chip-state-' + String(obligation.state),
        );
        span(obligationRow, obligation.origin, 'chip chip-origin');
        block.appendChild(obligationRow);
        var refs = Array.isArray(obligation.evidenceRefs) ? obligation.evidenceRefs : [];
        for (var e = 0; e < refs.length; e++) {
          line(block, 'evidence: ' + String(refs[e]), 'muted contract-evidence-ref');
        }
      }
    }
    var lines = Array.isArray(noteLines) ? noteLines : [];
    // The section's own note lines (dropped/unreadable rows) stay muted.
    for (var n = 0; n < lines.length; n++) {
      line(block, lines[n], 'muted');
    }
  }

  // ------------------------------------------------------- navigation shell
  //
  // ONE compact Work / Inspect / History switcher plus the Inspect subview
  // tablist (Overview / Plan / Changes / Verification / Agents). Both are
  // real tablists: role=tab controls, aria-selected + roving tabindex, and
  // ArrowLeft/ArrowRight/Home/End while a tab is focused. Selection is LOCAL
  // and stable — the host never switches views — and an Inspect subview
  // re-renders from the last snapshot when it becomes active. Work is the
  // default view and is never forced back by an incoming snapshot.

  var INSPECT_SUBVIEWS = [
    {
      key: 'overview',
      label: 'Overview',
      tab: 'subtab-overview',
      sections: [],
    },
    {
      key: 'plan',
      label: 'Plan',
      tab: 'subtab-plan',
      sections: ['plan', 'children', 'tournament', 'phase'],
    },
    { key: 'changes', label: 'Changes', tab: 'subtab-changes', sections: [] },
    {
      key: 'verification',
      label: 'Verification',
      tab: 'subtab-verification',
      sections: [
        'verification',
        'evidence',
        'acceptance',
        'contract',
        'proof',
        'integration',
        'phase',
        'spend',
        'blockers',
      ],
    },
    { key: 'agents', label: 'Agents', tab: 'subtab-agents', sections: [] },
  ];
  var CHANGES_EMPTY_LINE = 'No candidate changes are served for this run yet.';
  var MAX_CHANGED_PATHS = 200;
  var DIFF_UNAVAILABLE_LABEL = 'Diff unavailable';
  var DIFF_UNAVAILABLE_COPY =
    'The runtime does not serve an immutable candidate content read for this change.';
  var CHANGES_UNAVAILABLE_LABEL = 'Changes unavailable';
  var OUTCOMES_UNAVAILABLE_LABEL = 'Cost unavailable';
  var OUTCOMES_UNAVAILABLE_COPY = 'The current authority has not supplied a cost value.';
  var OUTCOMES_TRUNCATED_NOTE = 'Truncated sample';
  var MAX_OUTCOME_METRIC_ROWS = 32;
  var MAX_OUTCOME_RUN_ROWS = 200;
  // The History destination's two surfaces (I07): the Conversations tab is
  // the untouched listing; Outcomes is read LAZILY on an explicit open.
  var HISTORY_TABS = [
    { key: 'conversations', tab: 'history-tab-conversations', panel: 'history-conversations' },
    { key: 'outcomes', tab: 'history-tab-outcomes', panel: 'history-outcomes' },
    { key: 'account', tab: 'history-tab-account', panel: 'history-account' },
  ];

  function subviewOf(key) {
    for (var i = 0; i < INSPECT_SUBVIEWS.length; i++) {
      if (INSPECT_SUBVIEWS[i].key === key) {
        return INSPECT_SUBVIEWS[i];
      }
    }
    return INSPECT_SUBVIEWS[0];
  }

  function setTabSelected(id, selected) {
    var node = byId(id);
    if (!node || typeof node.setAttribute !== 'function') {
      return;
    }
    node.setAttribute('aria-selected', selected ? 'true' : 'false');
    node.setAttribute('tabindex', selected ? '0' : '-1');
  }

  /**
   * Roving-tabindex tablist wiring: click selects, and while a tab is
   * focused ArrowLeft/ArrowRight/Home/End move the selection exactly one
   * step with explicit focus following it. One active tab, stable selection.
   */
  function wireTablist(tabIds, select) {
    for (var i = 0; i < tabIds.length; i++) {
      (function (index) {
        var node = byId(tabIds[index]);
        if (!node || typeof node.addEventListener !== 'function') {
          return;
        }
        node.addEventListener('click', function () {
          select(tabIds[index], true);
        });
        node.addEventListener('keydown', function (event) {
          var key = event && event.key;
          var target = null;
          if (key === 'ArrowRight' || key === 'ArrowDown') {
            target = (index + 1) % tabIds.length;
          } else if (key === 'ArrowLeft' || key === 'ArrowUp') {
            target = (index - 1 + tabIds.length) % tabIds.length;
          } else if (key === 'Home') {
            target = 0;
          } else if (key === 'End') {
            target = tabIds.length - 1;
          }
          if (target === null) {
            return;
          }
          if (event.preventDefault) {
            event.preventDefault();
          }
          select(tabIds[target], true);
        });
      })(i);
    }
  }

  /**
   * ONE active destination at a time. Chat is the default; Inspect, History
   * and Diagnostics are full-window views opened from the overflow menu.
   * Selecting a destination closes the menu and never posts to the host.
   */
  function setActiveView(key) {
    if (!VIEW_PANELS[key]) {
      return;
    }
    activeView = key;
    var currentMenu = {
      work: 'menu-chat',
      inspect: 'menu-inspect',
      history: 'menu-history',
      settings: 'menu-settings',
      diagnostics: 'menu-diagnostics',
    };
    for (var panelKey in VIEW_PANELS) {
      if (!Object.prototype.hasOwnProperty.call(VIEW_PANELS, panelKey)) {
        continue;
      }
      var panel = byId(VIEW_PANELS[panelKey]);
      if (panel) {
        panel.hidden = panelKey !== key;
      }
      var item = byId(currentMenu[panelKey]);
      if (item && typeof item.setAttribute === 'function') {
        item.setAttribute('aria-current', panelKey === key ? 'page' : 'false');
      }
    }
    var back = byId('btn-back-chat');
    if (back) {
      // Settings renders its OWN chrome (settings-nav carries the back +
      // title): the header back stays hidden there so exactly one back
      // control exists per view (the E8 home button is gone entirely).
      back.hidden = key === 'work' || key === 'settings';
    }
    var settingsNav = byId('settings-nav');
    if (settingsNav) {
      settingsNav.hidden = key !== 'settings';
    }
    document.body.setAttribute('data-mode', key === 'settings' ? 'settings' : 'chat');
    setOverflowOpen(false);
    safeSetTimeout(function () {
      try {
        vscode.postMessage({
          type: 'uiDomDump',
          html: new XMLSerializer().serializeToString(document.documentElement).slice(0, 300000),
          mode: document.body.getAttribute('data-mode') + ':' + key,
          version: (byId('ui-version') || {}).textContent || '',
        });
      } catch (e) {}
    }, 400);
  }

  var MENU_ITEMS = [
    'menu-chat',
    'menu-run-action',
    'menu-inspect',
    'menu-history',
    'menu-settings',
    'menu-diagnostics',
    'menu-stop',
  ];

  function visibleMenuItems() {
    var items = [];
    for (var i = 0; i < MENU_ITEMS.length; i++) {
      var node = byId(MENU_ITEMS[i]);
      if (node && node.hidden !== true) {
        items.push(node);
      }
    }
    return items;
  }

  /**
   * Open a destination and move focus to its full-window region. Chat returns
   * to the composer; the other destinations focus the region itself so a
   * keyboard user never lands on a hidden menu item.
   */
  function openView(key) {
    setActiveView(key);
    var target = key === 'work' ? byId('goal') : byId(VIEW_PANELS[key]);
    if (target && typeof target.focus === 'function') {
      target.focus();
    }
  }

  function setOverflowOpen(open) {
    overflowOpen = open === true;
    var menu = byId('overflow-menu');
    if (menu) {
      menu.hidden = !overflowOpen;
    }
    var button = byId('btn-overflow');
    if (button && typeof button.setAttribute === 'function') {
      button.setAttribute('aria-expanded', overflowOpen ? 'true' : 'false');
    }
    if (overflowOpen) {
      var first = visibleMenuItems()[0];
      if (first && typeof first.focus === 'function') {
        first.focus();
      }
    } else if (menu && typeof menu.contains === 'function' && menu.contains(document.activeElement)) {
      if (button && typeof button.focus === 'function') {
        button.focus();
      }
    }
  }

  function toggleOverflow() {
    setOverflowOpen(!overflowOpen);
  }

  /** WAI-ARIA menu keys: arrows wrap, Home/End jump, Escape is global. */
  function wireMenuNavigation() {
    for (var i = 0; i < MENU_ITEMS.length; i++) {
      (function (id) {
        var node = byId(id);
        if (!node || typeof node.addEventListener !== 'function') {
          return;
        }
        node.addEventListener('keydown', function (event) {
          var key = event && event.key;
          if (
            key !== 'ArrowDown' &&
            key !== 'ArrowUp' &&
            key !== 'Home' &&
            key !== 'End'
          ) {
            return;
          }
          var items = visibleMenuItems();
          if (items.length === 0) {
            return;
          }
          var current = -1;
          for (var n = 0; n < items.length; n++) {
            if (items[n] === node) {
              current = n;
              break;
            }
          }
          var target = 0;
          if (key === 'ArrowDown') {
            target = current < 0 ? 0 : (current + 1) % items.length;
          } else if (key === 'ArrowUp') {
            target = current < 0 ? items.length - 1 : (current - 1 + items.length) % items.length;
          } else if (key === 'End') {
            target = items.length - 1;
          }
          if (event.preventDefault) {
            event.preventDefault();
          }
          if (typeof items[target].focus === 'function') {
            items[target].focus();
          }
        });
      })(MENU_ITEMS[i]);
    }
  }

  function subviewTabKey(tabId) {
    for (var i = 0; i < INSPECT_SUBVIEWS.length; i++) {
      if (INSPECT_SUBVIEWS[i].tab === tabId) {
        return INSPECT_SUBVIEWS[i].key;
      }
    }
    return null;
  }

  function selectSubviewTab(tabId, focusTab) {
    var key = subviewTabKey(tabId);
    if (key === null) {
      return;
    }
    setActiveSubview(key);
    if (focusTab === true) {
      var node = byId(tabId);
      if (node && typeof node.focus === 'function') {
        node.focus();
      }
    }
  }

  /** Mount the ONE shared cockpit card into the active content subpanel. */
  function mountInspectCard(subviewKey) {
    if (subviewKey !== 'overview' && subviewKey !== 'plan' && subviewKey !== 'verification') {
      return;
    }
    var card = byId('inspect-card');
    var panel = byId('inspect-subpanel-' + subviewKey);
    if (!card || !panel || typeof panel.appendChild !== 'function') {
      return;
    }
    if (card.parentNode !== panel) {
      if (card.parentNode && typeof card.parentNode.removeChild === 'function') {
        card.parentNode.removeChild(card);
      }
      panel.appendChild(card);
    }
  }

  function setActiveSubview(key) {
    var spec = subviewOf(key);
    activeSubview = spec.key;
    for (var i = 0; i < INSPECT_SUBVIEWS.length; i++) {
      var selected = INSPECT_SUBVIEWS[i].key === spec.key;
      setTabSelected(INSPECT_SUBVIEWS[i].tab, selected);
      var panel = byId('inspect-subpanel-' + INSPECT_SUBVIEWS[i].key);
      if (panel) {
        panel.hidden = !selected;
      }
    }
    mountInspectCard(spec.key);
    renderBreadcrumb(lastSnapshot);
    if (lastSnapshot) {
      renderInspectContent(lastSnapshot);
    }
  }

  /**
   * The same-scope breadcrumb: `Inspect / <Subview>` plus the current scope
   * (the workspace NAME — never a raw id). Session/run ids stay behind the
   * "Advanced diagnostics" disclosure, never in the primary line.
   */
  function renderBreadcrumb(snapshot) {
    var spec = subviewOf(activeSubview);
    var path = byId('inspect-breadcrumb-path');
    if (path) {
      var pathText = 'Inspect / ' + spec.label;
      if (path.textContent !== pathText) {
        path.textContent = pathText;
      }
    }
    var scope = byId('inspect-breadcrumb-scope');
    if (scope) {
      var name =
        snapshot && typeof snapshot.workspaceName === 'string' && snapshot.workspaceName.trim().length > 0
          ? snapshot.workspaceName.trim()
          : null;
      var scopeText = name !== null ? 'scope ' + name : 'scope this window';
      if (scope.textContent !== scopeText) {
        scope.textContent = scopeText;
      }
    }
    var diagnostics = byId('inspect-breadcrumb-diagnostics-body');
    if (diagnostics) {
      var sessionId = snapshot && snapshot.session ? String(snapshot.session.id) : null;
      var runId = snapshot && snapshot.activeRunId ? String(snapshot.activeRunId) : null;
      var diagnosticsText =
        'session ' + (sessionId === null ? 'none' : sessionId) + ' \u00b7 run ' + (runId === null ? 'none' : runId);
      if (diagnostics.textContent !== diagnosticsText) {
        diagnostics.textContent = diagnosticsText;
      }
    }
  }

  /**
   * Filter the served cockpit sections for one subview. Every section maps to
   * exactly one subview; an ADDITIVE section this panel version does not know
   * yet stays visible on Overview instead of being silently dropped. Section
   * content and wording are never altered here.
   */
  function sectionsForSubview(subviewKey, sections) {
    var spec = subviewOf(subviewKey);
    var list = Array.isArray(sections) ? sections : [];
    var known = {};
    for (var i = 0; i < INSPECT_SUBVIEWS.length; i++) {
      for (var j = 0; j < INSPECT_SUBVIEWS[i].sections.length; j++) {
        known[INSPECT_SUBVIEWS[i].sections[j]] = true;
      }
    }
    var out = [];
    for (var k = 0; k < list.length; k++) {
      var section = list[k];
      var key = section && typeof section.key === 'string' ? section.key : '';
      if (spec.sections.indexOf(key) !== -1) {
        out.push(section);
      } else if (spec.key === 'overview' && key.length > 0 && known[key] !== true) {
        out.push(section);
      }
    }
    return out;
  }

  /** Re-render every Inspect surface from the current snapshot + selection. */
  function renderInspectContent(snapshot) {
    if (!snapshot) {
      return;
    }
    renderCompletion(snapshot.task);
    renderCockpit(snapshot.cockpit, sectionsForSubview(activeSubview, snapshot.cockpitSections));
    renderChanges(snapshot);
    renderAgents(snapshot.agents);
    renderBoard(snapshot.board);
  }

  /**
   * The Changes surface (I03): the served `projection.filesChanged` rows with
   * only served refs, the scope toolbar of the selected change, the explicit
   * unavailable state on a failed projection read and the exact
   * `Diff unavailable` in-view state when no immutable basis is served.
   * Selection is LOCAL, keyed by path and preserved across refreshes.
   */
  function selectedIndexFor(rows) {
    if (!Array.isArray(rows) || rows.length === 0) {
      return -1;
    }
    if (selectedChangePath !== null) {
      for (var i = 0; i < rows.length; i++) {
        if (String(rows[i].path) === selectedChangePath) {
          return i;
        }
      }
    }
    if (!Number.isInteger(selectedChangeIndex) || selectedChangeIndex < 0) {
      return 0;
    }
    return Math.min(selectedChangeIndex, rows.length - 1);
  }

  function changeRowNode(row, index) {
    var item = document.createElement('li');
    var selected = index === selectedChangeIndex;
    item.className = 'changes-row' + (selected ? ' changes-row-selected' : '');
    item.setAttribute('data-change-path', String(row.path));
    var select = document.createElement('button');
    select.type = 'button';
    select.className = 'change-select';
    select.setAttribute('aria-pressed', selected ? 'true' : 'false');
    select.textContent = String(row.path);
    select.title = String(row.path);
    select.addEventListener('click', function () {
      selectChange(index, rowsFromSnapshot());
    });
    item.appendChild(select);
    if (typeof row.changeKind === 'string' && row.changeKind.length > 0) {
      span(item, 'kind ' + row.changeKind, 'chip change-kind');
    }
    var refs = Array.isArray(row.refs) ? row.refs : [];
    for (var r = 0; r < refs.length; r++) {
      var ref = refs[r] || {};
      line(item, String(ref.label == null ? '' : ref.label), 'muted change-ref');
    }
    return item;
  }

  function rowsFromSnapshot() {
    var changes = lastSnapshot && lastSnapshot.changes ? lastSnapshot.changes : null;
    return changes && Array.isArray(changes.rows) ? changes.rows : [];
  }

  /** Local selection move (rows are served, never fabricated). */
  function selectChange(index, rows) {
    if (!Array.isArray(rows) || rows.length === 0) {
      return;
    }
    if (index < 0) {
      index = 0;
    }
    if (index >= rows.length) {
      index = rows.length - 1;
    }
    selectedChangeIndex = index;
    selectedChangePath = String(rows[index].path);
    renderChanges(lastSnapshot);
  }

  function renderChangeToolbar(changes, row, index) {
    var candidate = byId('change-candidate');
    if (candidate) {
      var display =
        changes && typeof changes.candidateDisplay === 'string' && changes.candidateDisplay.length > 0
          ? changes.candidateDisplay
          : 'Candidate change';
      candidate.textContent = display;
    }
    var rows = changes && Array.isArray(changes.rows) ? changes.rows : [];
    var hasRow = row !== null && row !== undefined && index >= 0;
    var prev = byId('btn-change-prev');
    var next = byId('btn-change-next');
    var review = byId('btn-change-review');
    var claim = byId('btn-change-claim');
    if (prev) {
      prev.disabled = !hasRow || index <= 0;
    }
    if (next) {
      next.disabled = !hasRow || index >= rows.length - 1;
    }
    if (review) {
      review.disabled = !hasRow;
    }
    var criterionKey = null;
    if (hasRow) {
      var refs = Array.isArray(row.refs) ? row.refs : [];
      for (var i = 0; i < refs.length; i++) {
        if (refs[i] && typeof refs[i].criterionKey === 'string' && refs[i].criterionKey.length > 0) {
          criterionKey = refs[i].criterionKey;
          break;
        }
      }
    }
    if (claim) {
      claim.disabled = criterionKey === null;
    }
    var basis = byId('change-basis');
    if (basis) {
      clear(basis);
      var summary = changes && changes.basis ? changes.basis : null;
      if (summary && Array.isArray(summary.lines)) {
        for (var b = 0; b < summary.lines.length; b++) {
          line(basis, String(summary.lines[b]), 'change-basis-line');
        }
      }
    }
    var advancedBody = byId('change-advanced-body');
    if (advancedBody) {
      clear(advancedBody);
      var advanced = changes && Array.isArray(changes.advanced) ? changes.advanced : [];
      for (var a = 0; a < advanced.length; a++) {
        var fact = advanced[a] || {};
        line(advancedBody, String(fact.label == null ? '' : fact.label) + ': ' + String(fact.value == null ? '—' : fact.value), 'change-advanced-fact');
      }
    }
  }

  function renderChangeReviewState() {
    var node = byId('change-review-state');
    if (!node) {
      return;
    }
    var state = changeReviewState;
    if (!state || state.path !== selectedChangePath) {
      node.hidden = true;
      clear(node);
      return;
    }
    clear(node);
    node.hidden = false;
    if (state.state === 'unavailable') {
      line(node, DIFF_UNAVAILABLE_LABEL, 'change-review-label');
      line(node, DIFF_UNAVAILABLE_COPY, 'muted');
      if (typeof state.reason === 'string' && state.reason.length > 0) {
        line(node, state.reason, 'muted change-review-reason');
      }
    } else {
      line(
        node,
        'Immutable basis opened — evidence #' +
          String(state.evidenceId == null ? '—' : state.evidenceId) +
          ' (read-only).',
        'muted',
      );
    }
  }

  function renderChanges(snapshot) {
    var list = byId('changes-list');
    var empty = byId('changes-empty');
    var unavailable = byId('changes-unavailable');
    var toolbar = byId('changes-toolbar');
    if (!list || !empty || !unavailable || !toolbar) {
      return;
    }
    var changes = snapshot && snapshot.changes ? snapshot.changes : null;
    if (!changes) {
      changes = {
        available: true,
        reason: null,
        rows: [],
        candidateDisplay: 'Candidate change',
        basis: null,
        advanced: [],
      };
    }
    // Content guard: everything this renderer puts on screen is a pure
    // function of (changes, normalized selection, review state). An
    // identical delta re-renders NOTHING — the previous DOM is already what
    // a rebuild would produce.
    var guardRows = Array.isArray(changes.rows) ? changes.rows : [];
    var guardIndex = selectedIndexFor(guardRows);
    var guardKey =
      hash128(jsonForHash(changes)) +
      '#' +
      guardIndex +
      '#' +
      (guardIndex >= 0 ? String(guardRows[guardIndex].path) : '') +
      '#' +
      hash128(jsonForHash(changeReviewState));
    if (renderedChangesKey !== null && renderedChangesKey === guardKey) {
      return;
    }
    renderedChangesKey = guardKey;
    renderProbe('changes');
    var focus = captureFocus(list);
    clear(list);
    if (changes.available === false) {
      // Explicit unavailable state with the exact reason; no rows and no
      // stale previously served paths (the read failure drops them).
      renderedChangesKey = null;
      setText('changes-unavailable-label', CHANGES_UNAVAILABLE_LABEL);
      setText(
        'changes-unavailable-copy',
        typeof changes.reason === 'string' && changes.reason.length > 0
          ? 'The changed-path read failed: ' + changes.reason
          : 'The changed-path read failed and no reason was supplied.',
      );
      unavailable.hidden = false;
      toolbar.hidden = true;
      list.hidden = true;
      empty.hidden = true;
      var review = byId('change-review-state');
      if (review) {
        review.hidden = true;
      }
      return;
    }
    unavailable.hidden = true;
    toolbar.hidden = false;
    list.hidden = false;
    var rows = Array.isArray(changes.rows) ? changes.rows : [];
    var index = selectedIndexFor(rows);
    selectedChangeIndex = index;
    selectedChangePath = index >= 0 ? String(rows[index].path) : null;
    if (rows.length === 0) {
      empty.hidden = false;
      if (empty.textContent !== CHANGES_EMPTY_LINE) {
        empty.textContent = CHANGES_EMPTY_LINE;
      }
      renderChangeToolbar(changes, null, -1);
      renderChangeReviewState();
      return;
    }
    empty.hidden = true;
    var shown = rows.slice(0, MAX_CHANGED_PATHS);
    for (var i = 0; i < shown.length; i++) {
      list.appendChild(changeRowNode(shown[i], i));
    }
    if (rows.length > shown.length) {
      var more = document.createElement('li');
      more.className = 'changes-path muted';
      more.textContent = '... ' + (rows.length - shown.length) + ' more';
      list.appendChild(more);
    }
    renderChangeToolbar(changes, rows[index], index);
    renderChangeReviewState();
    restoreFocus(list, focus);
  }

  /**
   * `Open claim`: switch to Inspect / Verification and select the related
   * criterion row when it is served in the current verification view. When
   * the criterion is not served there, say so — never fabricate a row.
   */
  function openClaimForSelected() {
    var rows = rowsFromSnapshot();
    var index = selectedIndexFor(rows);
    if (index < 0) {
      return;
    }
    var refs = Array.isArray(rows[index].refs) ? rows[index].refs : [];
    var key = null;
    for (var i = 0; i < refs.length; i++) {
      if (refs[i] && typeof refs[i].criterionKey === 'string' && refs[i].criterionKey.length > 0) {
        key = refs[i].criterionKey;
        break;
      }
    }
    if (key === null) {
      return;
    }
    setActiveView('inspect');
    setActiveSubview('verification');
    var found = null;
    var cards =
      typeof document.querySelectorAll === 'function'
        ? document.querySelectorAll('[data-criterion-key]')
        : [];
    for (var c = 0; c < cards.length; c++) {
      if (!cards[c].getAttribute || cards[c].getAttribute('data-criterion-key') !== key) {
        continue;
      }
      // Prefer the VISIBLE card (the same criterion can render in the
      // Overview panel too; the hidden one must never take focus).
      var cursor = cards[c];
      var hiddenAncestor = false;
      while (cursor) {
        if (cursor.hidden === true) {
          hiddenAncestor = true;
          break;
        }
        cursor = cursor.parentNode;
      }
      if (!hiddenAncestor) {
        found = cards[c];
        break;
      }
      if (found === null) {
        found = cards[c];
      }
    }
    if (found === null) {
      showNotice('info', 'the related criterion is not served in the current verification view');
      return;
    }
    found.classList.add('criterion-selected');
    found.setAttribute('tabindex', '-1');
    if (typeof found.focus === 'function') {
      found.focus();
    }
  }

  // ------------------------------------------------------------- history
  //
  // The History destination renders ONLY the served durable listing: real
  // title, served state and the served created_ms date (an explicit "date
  // unavailable" when the daemon serves none). A read failure renders the
  // T31 copy with a Refresh history action and NO rows. Row Open posts ONE
  // `openSession`; the host performs the single switch path.

  function captureHistoryFocus() {
    var active = typeof document !== 'undefined' ? document.activeElement : null;
    if (!active) {
      return null;
    }
    var activeId =
      active.id !== undefined && active.id !== ''
        ? active.id
        : active.getAttribute
          ? active.getAttribute('id')
          : null;
    if (activeId === 'history-search') {
      return { kind: 'search' };
    }
    var cursor = active;
    while (cursor) {
      if (cursor.getAttribute && cursor.getAttribute('data-session-id')) {
        return { kind: 'open', sessionId: cursor.getAttribute('data-session-id') };
      }
      cursor = cursor.parentNode;
    }
    return null;
  }

  /** Restore focus onto the SAME logical row/button after an async refresh. */
  function restoreHistoryFocus(descriptor) {
    if (!descriptor) {
      return;
    }
    if (descriptor.kind === 'search') {
      var search = byId('history-search');
      if (search && typeof search.focus === 'function') {
        search.focus();
      }
      return;
    }
    if (descriptor.kind === 'open' && /^[0-9]+$/.test(String(descriptor.sessionId))) {
      var list = byId('history-list');
      var row =
        list && list.querySelector
          ? list.querySelector('[data-session-id="' + descriptor.sessionId + '"]')
          : null;
      var button = row && row.querySelector ? row.querySelector('.history-open') : null;
      if (button && typeof button.focus === 'function') {
        button.focus();
      }
    }
  }

  function historyRowNode(snapshot, entry) {
    var id = entry && entry.id !== undefined && entry.id !== null ? String(entry.id) : '';
    var item = document.createElement('li');
    item.className = 'history-row';
    if (id.length > 0) {
      item.setAttribute('data-session-id', id);
    }
    var main = document.createElement('div');
    main.className = 'history-row-main';
    var title = span(main, historyPolicy.historyTitle(entry), 'history-row-title');
    span(main, historyPolicy.historyStateLabel(entry), 'chip history-row-state');
    if (snapshot && snapshot.session && String(snapshot.session.id) === id) {
      span(main, 'current', 'chip history-row-current');
    }
    item.appendChild(main);
    var metaBits = [];
    var date = historyPolicy.sessionDateLabel(entry ? entry.createdMs : null);
    metaBits.push(date === null ? 'date unavailable' : 'created ' + date);
    var provider = entry && typeof entry.provider === 'string' ? entry.provider : '';
    var model = entry && typeof entry.model === 'string' ? entry.model : '';
    if (provider.length > 0 || model.length > 0) {
      metaBits.push(model.length > 0 ? (provider.length > 0 ? provider + ' / ' + model : model) : provider);
    }
    line(item, metaBits.join(' \u00b7 '), 'muted history-row-meta');
    var open = document.createElement('button');
    open.type = 'button';
    open.className = 'secondary history-open';
    open.textContent = 'Open';
    open.setAttribute('aria-label', 'Open conversation ' + title.textContent);
    open.disabled = historyOpenInFlight !== null && historyOpenInFlight === id;
    open.addEventListener('click', function () {
      // Single-flight per logical row: a double click can never emit two
      // openSession posts for the same conversation.
      if (id.length === 0 || historyOpenInFlight === id) {
        return;
      }
      historyOpenInFlight = id;
      open.disabled = true;
      vscode.postMessage({ type: 'openSession', sessionId: id });
    });
    item.appendChild(open);
    return item;
  }

  function renderHistory(snapshot) {
    if (snapshot) {
      lastSnapshot = snapshot;
    }
    if (!lastSnapshot) {
      return;
    }
    snapshot = lastSnapshot;
    var list = byId('history-list');
    var status = byId('history-status');
    var note = byId('history-note');
    var controls = byId('history-controls');
    var unavailable = byId('history-unavailable');
    if (!list || !status || !note || !controls || !unavailable) {
      return;
    }
    if (note.textContent !== historyPolicy.HISTORY_SEPARATION_NOTE) {
      note.textContent = historyPolicy.HISTORY_SEPARATION_NOTE;
    }
    var readFailed =
      typeof snapshot.sessionsUnavailable === 'string' && snapshot.sessionsUnavailable.length > 0;
    var serviceDown = snapshot.daemon !== 'running';
    if (readFailed || serviceDown) {
      setText('history-unavailable-label', historyPolicy.HISTORY_UNAVAILABLE_LABEL);
      setText('history-unavailable-copy', historyPolicy.HISTORY_UNAVAILABLE_COPY);
      setText('btn-history-refresh-unavailable', historyPolicy.HISTORY_UNAVAILABLE_ACTION);
      unavailable.hidden = false;
      controls.hidden = true;
      status.hidden = true;
      note.hidden = true;
      list.hidden = true;
      historyOpenInFlight = null;
      renderedHistoryKey = null;
      clear(list);
      return;
    }
    unavailable.hidden = true;
    controls.hidden = false;
    status.hidden = false;
    note.hidden = false;
    list.hidden = false;
    if (
      historyOpenInFlight !== null &&
      snapshot.session &&
      String(snapshot.session.id) === historyOpenInFlight
    ) {
      historyOpenInFlight = null;
    }
    // Content guard: the rows derive from (sessions, query) through the
    // history policy and the framing flags are part of the key, so an
    // identical delta never rebuilds up to a full 1000-row listing.
    var historyKey =
      hash128(jsonForHash(Array.isArray(snapshot.sessions) ? snapshot.sessions : [])) +
      '#' +
      hash128(historyQuery) +
      '#' +
      (snapshot.session ? String(snapshot.session.id) : '') +
      '#' +
      String(historyOpenInFlight);
    if (renderedHistoryKey !== null && renderedHistoryKey === historyKey) {
      return;
    }
    renderedHistoryKey = historyKey;
    var result = historyPolicy.historyRows(
      Array.isArray(snapshot.sessions) ? snapshot.sessions : [],
      historyQuery,
    );
    status.textContent = historyPolicy.historyStatusText(result, historyQuery);
    var focus = captureHistoryFocus();
    renderProbe('history');
    clear(list);
    for (var i = 0; i < result.rows.length; i++) {
      list.appendChild(historyRowNode(snapshot, result.rows[i]));
    }
    restoreHistoryFocus(focus);
  }

  // ---------------------------------------------------------- outcomes (I07)
  //
  // The Engineering Outcome Ledger surface. It renders ONLY the host's
  // projection of the served page: every metric keeps its served counts and
  // the host already applied the honesty rules (`No verified changes yet`,
  // `not measured`, exact money strings). The read is LAZY: switching to the
  // Outcomes tab posts ONE `readOutcomes`; the normal conversation snapshot
  // never triggers a ledger read.

  function outcomeMetricNode(metric) {
    var row = document.createElement('div');
    var group = typeof metric.group === 'string' ? metric.group : 'rate';
    var cohort = typeof metric.cohort === 'string' && metric.cohort.length > 0 ? metric.cohort : null;
    row.className =
      'outcome-metric outcome-metric-' +
      group +
      (cohort !== null ? ' outcome-cohort-' + cohort : '') +
      (metric.unmeasured === true ? ' outcome-metric-unmeasured' : '');
    row.setAttribute('data-metric', String(metric.key == null ? '' : metric.key));
    var head = document.createElement('div');
    head.className = 'outcome-metric-head';
    span(head, String(metric.label == null ? '' : metric.label), 'outcome-metric-label');
    span(head, String(metric.value == null ? '—' : metric.value), 'outcome-metric-value');
    row.appendChild(head);
    line(row, String(metric.detail == null ? '' : metric.detail), 'muted outcome-metric-detail');
    var explain = document.createElement('details');
    explain.className = 'technical-disclosure outcome-metric-explain';
    var summary = document.createElement('summary');
    summary.textContent = 'Explain metric';
    explain.appendChild(summary);
    line(explain, String(metric.definition == null ? '' : metric.definition), 'muted');
    row.appendChild(explain);
    return row;
  }

  function outcomeRunNode(run) {
    var item = document.createElement('li');
    item.className = 'outcome-run ' + (run.verified === true ? 'outcome-run-verified' : 'outcome-run-unverified');
    var head = document.createElement('div');
    head.className = 'outcome-run-head';
    span(head, String(run.verdict == null ? '' : run.verdict), 'chip outcome-run-verdict');
    span(head, String(run.completedText == null ? '—' : run.completedText), 'muted outcome-run-date');
    span(head, String(run.modelSpendUsd == null ? '—' : run.modelSpendUsd), 'outcome-run-cost');
    span(head, String(run.wallText == null ? '—' : run.wallText), 'muted outcome-run-wall');
    item.appendChild(head);
    var facts = [
      'class ' + String(run.taskClass == null ? '—' : run.taskClass),
      'risk ' + String(run.riskClass == null ? '—' : run.riskClass),
      run.firstVerificationPass === true
        ? 'first-verification-pass verified'
        : String(run.repairAttempts == null ? 0 : run.repairAttempts) + ' repair attempt(s)',
      'files ' + String(run.changeFiles == null ? 0 : run.changeFiles),
      'steps ' + String(run.steps == null ? '—' : run.steps),
    ];
    line(item, facts.join(' · '), 'muted outcome-run-meta');
    var details = document.createElement('details');
    details.className = 'technical-disclosure';
    var summary = document.createElement('summary');
    summary.textContent = 'Advanced details';
    details.appendChild(summary);
    line(
      details,
      'run ' +
        String(run.runId == null ? '—' : run.runId) +
        ' · task ' +
        String(run.taskId == null ? '—' : run.taskId) +
        ' · session ' +
        String(run.sessionId == null ? '—' : run.sessionId),
      'muted outcome-run-ids',
    );
    line(
      details,
      'model spend ' +
        String(run.modelSpendExact == null ? '—' : run.modelSpendExact) +
        ' · verification ' +
        String(run.verificationSpendExact == null ? '—' : run.verificationSpendExact) +
        ' · rework ' +
        String(run.reworkSpendExact == null ? '—' : run.reworkSpendExact) +
        ' · avoided hosted ' +
        String(run.avoidedHostedExact == null ? '—' : run.avoidedHostedExact),
      'muted outcome-run-spend',
    );
    line(
      details,
      'interventions ' +
        String(run.humanInterventions == null ? 0 : run.humanInterventions) +
        ' · rollbacks ' +
        String(run.rollbackEvents == null ? 0 : run.rollbackEvents) +
        ' · recoveries ' +
        String(run.recoveryEvents == null ? 0 : run.recoveryEvents) +
        ' · frontier escalations ' +
        String(run.frontierEscalations == null ? 0 : run.frontierEscalations),
      'muted outcome-run-events',
    );
    item.appendChild(details);
    return item;
  }

  function renderOutcomes(snapshot) {
    var card = byId('outcomes-card');
    if (!card) {
      return;
    }
    if (snapshot) {
      lastSnapshot = snapshot;
    }
    var view = lastSnapshot ? lastSnapshot.outcomes : null;
    outcomesReading = false;
    // The host re-posts a fresh object graph per stream delta, so identity is
    // not a guard; the serialized ledger content is (it only changes on an
    // explicit outcomes read).
    var outcomesKey = view === null || view === undefined ? 'none' : hash128(jsonForHash(view));
    if (renderedOutcomesKey !== null && renderedOutcomesKey === outcomesKey) {
      updateOutcomesControls(view);
      return;
    }
    renderedOutcomesKey = outcomesKey;
    renderedOutcomes = view;
    var metrics = byId('outcomes-metrics');
    var runs = byId('outcomes-runs');
    var status = byId('outcomes-status');
    var entitlements = byId('outcomes-entitlements');
    var unavailable = byId('outcomes-unavailable');
    var truncated = byId('outcomes-truncated');
    if (!metrics || !runs || !status || !unavailable || !truncated) {
      return;
    }
    // The value-entitlements status line: rendered verbatim from the host's
    // projection; hidden while no organization is configured (null).
    var renderEntitlementsLine = function (view) {
      if (!entitlements) {
        return;
      }
      var text =
        view && typeof view.entitlementsStatus === 'string' ? view.entitlementsStatus : '';
      entitlements.textContent = text;
      entitlements.hidden = text.length === 0;
    };
    if (view === null || view === undefined) {
      status.textContent = 'Outcomes have not been read yet.';
      metrics.hidden = true;
      runs.hidden = true;
      truncated.hidden = true;
      unavailable.hidden = true;
      renderEntitlementsLine(null);
      updateOutcomesControls(null);
      return;
    }
    if (view.state === 'unavailable') {
      // T29: a distinct cost-unavailable state; NO monetary value renders.
      setText('outcomes-unavailable-label', OUTCOMES_UNAVAILABLE_LABEL);
      setText('outcomes-unavailable-copy', OUTCOMES_UNAVAILABLE_COPY);
      var detail = byId('outcomes-unavailable-detail');
      if (detail) {
        detail.textContent =
          (typeof view.reasonCode === 'string' && view.reasonCode.length > 0
            ? '[' + view.reasonCode + '] '
            : '') + String(view.reason == null ? 'no further detail was supplied' : view.reason);
      }
      unavailable.hidden = false;
      status.textContent = 'the outcome ledger read failed';
      status.hidden = false;
      metrics.hidden = true;
      runs.hidden = true;
      truncated.hidden = true;
      clear(metrics);
      clear(runs);
      renderEntitlementsLine(view);
      updateOutcomesControls(view);
      return;
    }
    unavailable.hidden = true;
    status.hidden = false;
    status.textContent =
      String(view.scanned == null ? 0 : view.scanned) +
      ' run(s) scanned · ' +
      String(view.metrics && view.runs ? view.runs.length : 0) +
      ' run row(s) loaded · page ' +
      String(view.pageCount == null ? 1 : view.pageCount);
    truncated.hidden = view.truncated !== true;
    setText('outcomes-truncated', OUTCOMES_TRUNCATED_NOTE);
    metrics.hidden = false;
    runs.hidden = false;
    clear(metrics);
    var metricRows = Array.isArray(view.metrics) ? view.metrics : [];
    if (metricRows.length > 0 && typeof view.costNote === 'string' && view.costNote.length > 0) {
      line(metrics, view.costNote, 'muted outcomes-cost-note');
    }
    for (var i = 0; i < metricRows.length && i < MAX_OUTCOME_METRIC_ROWS; i++) {
      metrics.appendChild(outcomeMetricNode(metricRows[i]));
    }
    clear(runs);
    var runRows = Array.isArray(view.runs) ? view.runs : [];
    for (var r = 0; r < runRows.length && r < MAX_OUTCOME_RUN_ROWS; r++) {
      runs.appendChild(outcomeRunNode(runRows[r]));
    }
    renderEntitlementsLine(view);
    updateOutcomesControls(view);
  }

  function updateOutcomesControls(view) {
    var refresh = byId('btn-outcomes-refresh');
    var older = byId('btn-outcomes-older');
    if (refresh) {
      refresh.disabled = outcomesReading;
    }
    if (older) {
      var hasOlder =
        view !== null &&
        view !== undefined &&
        view.state === 'ok' &&
        view.nextCursor !== null &&
        view.nextCursor !== undefined;
      older.hidden = !hasOlder;
      older.disabled = outcomesReading || !hasOlder;
    }
  }

  function setHistoryTab(key, focusTab) {
    var spec = null;
    for (var i = 0; i < HISTORY_TABS.length; i++) {
      if (HISTORY_TABS[i].key === key) {
        spec = HISTORY_TABS[i];
      }
    }
    if (spec === null) {
      return;
    }
    activeHistoryTab = spec.key;
    for (var t = 0; t < HISTORY_TABS.length; t++) {
      setTabSelected(HISTORY_TABS[t].tab, HISTORY_TABS[t].key === spec.key);
      var panel = byId(HISTORY_TABS[t].panel);
      if (panel) {
        panel.hidden = HISTORY_TABS[t].key !== spec.key;
      }
    }
    if (focusTab === true) {
      var node = byId(spec.tab);
      if (node && typeof node.focus === 'function') {
        node.focus();
      }
    }
    if (spec.key === 'outcomes') {
      // Opening the tab IS the explicit request: one bounded ledger read.
      outcomesReading = true;
      updateOutcomesControls(lastSnapshot ? lastSnapshot.outcomes : null);
      vscode.postMessage({ type: 'readOutcomes' });
      renderOutcomes(lastSnapshot);
    } else if (spec.key === 'account') {
      // Commercial data is lazy: this explicit open is the only trigger.
      vscode.postMessage({ type: 'readAccount' });
      renderAccount(lastSnapshot);
    }
  }

  function renderTask(snapshot) {
    var task = snapshot ? snapshot.task : null;
    var spec = runStateSpec(snapshot);
    // Content guard: the status row is a pure function of the task, the run
    // strip source, the agent/approval counts and the proof's embedded state.
    // A stream delta that only grows transcript text must not rebuild it.
    var guardKey =
      (spec === null ? 'none' : spec.label) +
      '#' +
      (spec && spec.action ? spec.action.key : 'none') +
      '#' +
      hash128(jsonForHash(task)) +
      '#' +
      hash128(jsonForHash(snapshot ? snapshot.agents : null)) +
      '#' +
      hash128(jsonForHash(snapshot ? snapshot.permissions : null)) +
      '#' +
      String(snapshot ? snapshot.proofTaskState : null) +
      '#' +
      String(snapshot ? snapshot.activeRunId : null) +
      '#' +
      String(snapshot ? snapshot.busy : null);
    if (renderedTaskKey !== null && renderedTaskKey === guardKey) {
      return;
    }
    renderedTaskKey = guardKey;
    renderProbe('task');
    // The product header carries the run status; there is no second status
    // row in the chat body. Run detail (goal, tokens, plan, candidate
    // boundary) lives in Inspect, and Stop lives in the overflow menu.
    var hasRun =
      task !== null ||
      (snapshot ? snapshot.busy === true || snapshot.activeRunId != null : false);
    var stopItem = byId('menu-stop');
    if (stopItem) {
      stopItem.hidden = !hasRun;
    }
    var action = spec !== null && spec.action ? spec.action : null;
    activeMenuAction = action;
    var menuAction = byId('menu-run-action');
    if (menuAction) {
      menuAction.hidden = action === null;
      if (action !== null) {
        menuAction.textContent = action.label;
        menuAction.setAttribute('data-ux-action', action.key);
      }
    }
    renderTaskProgress(task);
    renderTaskTests(task);
    renderTaskAgents(snapshot ? snapshot.agents : null);
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
    if (
      value === 'waiting' ||
      value === 'pending' ||
      value === 'paused' ||
      value === 'blocked' ||
      value === 'verifying' ||
      value === 'needs_verification'
    ) {
      return 'waiting';
    }
    return 'unknown';
  }

  /**
   * One state strip: glyph + label + supporting copy, text-first (the glyph
   * is never the only signal). A null spec hides the strip and leaves the
   * last text in place; nothing is rendered for an unserved condition.
   */
  function writeStateStrip(ids, spec) {
    var container = byId(ids.container);
    if (!container) {
      return;
    }
    container.hidden = spec === null;
    if (spec === null) {
      return;
    }
    // Same-value writes are skipped: an unchanged row must not re-announce
    // itself through the strip's live region on every snapshot.
    var glyph = byId(ids.glyph);
    if (glyph && glyph.textContent !== spec.glyph) {
      glyph.textContent = spec.glyph;
    }
    var label = byId(ids.label);
    if (label && label.textContent !== spec.label) {
      label.textContent = spec.label;
    }
    var copy = byId(ids.copy);
    if (copy && copy.textContent !== spec.copy) {
      copy.textContent = spec.copy;
    }
  }

  /**
   * The served run-state strip (T09/T12/T25/T26). Every source is genuinely
   * served, and no source can fabricate a row:
   *   - the newest durable task-run row (`snapshot.runs[0]`, the same row the
   *     host reads the proof for): `cancelled` -> T25, `failed` -> T26,
   *     `running` -> T09;
   *   - the proof body's embedded full task state (`snapshot.proofTaskState`,
   *     from `GET /native/tasks/{id}/proof`): `verifying` /
   *     `needs_verification` -> T12, ahead of T09 because the run row of a
   *     verifying task is still served as `running`;
   *   - the collapsed session tag may keep T09 while it reads `running`.
   * The collapsed session tag alone (`running|in_progress|done|idle`) can
   * never render T12/T25/T26; any other served value renders nothing new —
   * the chip above already names it. The useful actions stay the existing
   * controls (Inspect, Cancel run, the conversation and New task).
   */
  function runStateSpec(snapshot) {
    var ux = snapshot && snapshot.ux ? snapshot.ux : null;
    if (ux) {
      return {
        glyph: glyphForPresentation(ux.presentation),
        label: ux.presentationLabel,
        copy: ux.presentationDetail,
        action: ux.presentationAction || null,
      };
    }
    // Compatibility for snapshots produced before the shared UX projection
    // exists (older harnesses and cached snapshots). The served run sources
    // and precedence are unchanged; provider failure never says "resend".
    var runs = snapshot && Array.isArray(snapshot.runs) ? snapshot.runs : [];
    var newest = runs.length > 0 ? runs[0] : null;
    var runState =
      newest && newest.state != null ? String(newest.state).trim().toLowerCase() : '';
    if (runState === 'cancelled') return RUN_STATES.cancelled;
    if (runState === 'failed') return RUN_STATES.failed;
    var proofState = String(
      snapshot && snapshot.proofTaskState != null ? snapshot.proofTaskState : '',
    )
      .trim()
      .toLowerCase();
    if (proofState === 'verifying' || proofState === 'needs_verification') {
      return RUN_STATES.checking;
    }
    if (runState === 'verifying' || runState === 'needs_verification') {
      return RUN_STATES.checking;
    }
    if (runState === 'blocked' || runState === 'waiting' || runState === 'pending') {
      return RUN_STATES.needsInput;
    }
    if (runState === 'paused') return RUN_STATES.paused;
    if (runState === 'running') return RUN_STATES.running;
    var tag = String(
      snapshot && snapshot.task && snapshot.task.state != null ? snapshot.task.state : '',
    )
      .trim()
      .toLowerCase();
    if (tag === 'running') return RUN_STATES.running;
    return null;
  }

  function glyphForPresentation(presentation) {
    switch (String(presentation || '')) {
      case 'Starting':
      case 'Planning':
      case 'Working':
        return '\u25b6';
      case 'AwaitingUser':
      case 'Paused':
      case 'Failed':
        return '!';
      case 'Checking':
        return '\u2026';
      case 'Done':
        return '\u2713';
      default:
        return '\u2713';
    }
  }

  /**
   * The composer state strip (T06/T07/T08/T11). Precedence follows the
   * served signals: an in-flight submission is being admitted once; a kept
   * failed start stays until the next submission; a served
   * `needs_user_input` machine state asks for a reply in the composer; the
   * READY row renders only from the served ready preflight with no active
   * run. Nothing renders when no served condition holds.
   */
  function renderComposerState() {
    var spec = null;
    if (submitting) {
      spec = COMPOSER_STATES.starting;
    } else if (startFailed) {
      spec = COMPOSER_STATES.startUnavailable;
    } else if (machineStateValue === 'needs_user_input') {
      spec = COMPOSER_STATES.needsInput;
    } else if (lastPreflight && lastPreflight.state === 'ready' && !runActive) {
      spec = COMPOSER_STATES.ready;
    }
    writeStateStrip(COMPOSER_STATE_IDS, spec);
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

  /**
   * Display-safe approval text: invisible C0/C1 controls, bidi overrides and
   * zero-width joiners can remap what the human reads, so they never reach a
   * decision card. The gated native action itself is unchanged.
   */
  function permissionVisible(value) {
    return String(value)
      .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, '')
      .replace(/[\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/g, '');
  }

  /** The parsed detail payload ONLY when it is a JSON object. */
  function permissionDetailObject(detail) {
    if (typeof detail !== 'string' || detail.length === 0 || detail.charAt(0) !== '{') {
      return null;
    }
    var parsed;
    try {
      parsed = JSON.parse(detail);
    } catch (error) {
      return null;
    }
    if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
      return null;
    }
    return parsed;
  }

  /**
   * The REQUESTER-STATED reason of one native detail payload: an explicit
   * `reason`/`purpose`/`why` string member of a JSON OBJECT. It is agent
   * text, rendered with an explicit unverified label by the card; nothing is
   * inferred from other fields or from prose.
   */
  function permissionStatedReason(detail) {
    return cachedRender('reason', hash128(String(detail)), String(detail), function () {
      var parsed = permissionDetailObject(detail);
      if (parsed === null) {
        return null;
      }
      var keys = ['reason', 'purpose', 'why'];
      for (var i = 0; i < keys.length; i++) {
        var value = parsed[keys[i]];
        if (typeof value === 'string' && value.trim().length > 0) {
          return permissionVisible(value).trim().slice(0, MAX_PERMISSION_REASON_CHARS);
        }
      }
      return null;
    });
  }

  /** Human decision headline: what the agent wants to do, and to what. */
  function permissionHeadline(permission) {
    var capability = permissionVisible(permission.capability == null ? '' : permission.capability);
    var kind = capability.toLowerCase();
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
      what = 'Use ' + (capability.length > 0 ? capability : 'a capability');
    }
    var detail = permissionVisible(permission.detail == null ? '' : permission.detail);
    // Only a TOP-LEVEL string member of the detail object is a trustworthy
    // target. A regex over raw prose could promote a nested/planted key and
    // display a different command than the one the daemon gated.
    var target = null;
    var parsed = permissionDetailObject(permission.detail);
    var targetKeys = ['command', 'path', 'destination', 'tool'];
    if (parsed !== null) {
      for (var k = 0; k < targetKeys.length && target === null; k++) {
        var value = parsed[targetKeys[k]];
        if (typeof value === 'string' && value.length > 0) {
          target = permissionVisible(value).slice(0, 200);
        }
      }
    }
    return {
      headline: target ? what + ': ' + target : what,
      target: target,
      detail: detail,
      reason: permissionStatedReason(permission.detail),
    };
  }

  /** One approval decision button; both buttons gate on the request lock. */
  function permissionButton(permission, decision, label, secondary) {
    var id = String(permission.id == null ? '' : permission.id);
    var button = document.createElement('button');
    button.type = 'button';
    if (secondary) {
      button.className = 'secondary';
    }
    button.textContent = label;
    button.setAttribute('aria-label', label + ' permission request ' + id);
    button.disabled = permissionInFlight.has(id);
    button.addEventListener('click', function () {
      // The in-flight lock gates BEFORE the post: a double click, an
      // Enter+click race or a hostile burst can never emit a second reply
      // for the same request. It is released ONLY by that request's
      // explicit host result (permissionResult).
      if (permissionInFlight.has(id)) {
        return;
      }
      permissionInFlight.add(id);
      if (permissionInFlight.size > MAX_PENDING_PERMISSIONS) {
        // Set preserves insertion order: evict the oldest id, like shift().
        var oldest = permissionInFlight.values().next().value;
        permissionInFlight.delete(oldest);
      }
      setPermissionDecisionsDisabled(id, true);
      vscode.postMessage({
        type: 'permissionReply',
        permissionId: id,
        decision: decision,
      });
    });
    return button;
  }

  /** The rendered card item of one request id, or null. */
  function permissionItemById(id) {
    var list = byId('permission-list');
    if (!list) {
      return null;
    }
    for (var i = 0; i < list.children.length; i++) {
      var item = list.children[i];
      if (item.getAttribute && item.getAttribute('data-permission-id') === id) {
        return item;
      }
    }
    return null;
  }

  /** Disable/enable BOTH decision buttons of one request (never global). */
  function setPermissionDecisionsDisabled(id, disabled) {
    var item = permissionItemById(id);
    var actions = item && item.querySelector ? item.querySelector('.permission-actions') : null;
    if (!actions) {
      return;
    }
    for (var i = 0; i < actions.children.length; i++) {
      var child = actions.children[i];
      if (child.tagName === 'button') {
        child.disabled = disabled;
      }
    }
  }

  /** The refusal reason currently displayed for one request id, or null. */
  function permissionRefusalFor(id) {
    for (var i = 0; i < permissionRefusals.length; i++) {
      if (permissionRefusals[i].id === id) {
        return permissionRefusals[i].reason;
      }
    }
    return null;
  }

  function rememberPermissionRefusal(id, reason) {
    var next = [];
    for (var i = 0; i < permissionRefusals.length; i++) {
      if (permissionRefusals[i].id !== id) {
        next.push(permissionRefusals[i]);
      }
    }
    next.push({ id: id, reason: reason });
    permissionRefusals = next.slice(Math.max(0, next.length - MAX_PERMISSION_REFUSALS));
  }

  function forgetPermissionRefusal(id) {
    var next = [];
    for (var i = 0; i < permissionRefusals.length; i++) {
      if (permissionRefusals[i].id !== id) {
        next.push(permissionRefusals[i]);
      }
    }
    permissionRefusals = next;
  }

  /** Drop refusal state for requests no longer pending; keep it bounded. */
  function prunePermissionRefusals(items) {
    var live = new Set();
    for (var i = 0; i < items.length; i++) {
      live.add(String(items[i].id == null ? '' : items[i].id));
    }
    var next = [];
    for (var j = 0; j < permissionRefusals.length; j++) {
      if (live.has(permissionRefusals[j].id)) {
        next.push(permissionRefusals[j]);
      }
    }
    permissionRefusals = next;
  }

  /**
   * The approval card list. The card is driven by the SAME snapshot data the
   * reply path resolves against; an empty list hides the card entirely.
   * The subline is the AUTHORITY reason when the detail serves one and the
   * exact `Action requested for this run.` payload line otherwise — never a
   * fabricated purpose. A refused reply keeps its exact reason on the card.
   */
  function renderPermissions(permissions) {
    var card = byId('permission-card');
    var list = byId('permission-list');
    if (!card || !list) {
      return;
    }
    var items = Array.isArray(permissions) ? permissions.slice(0, MAX_PENDING_PERMISSIONS) : [];
    lastPermissions = items;
    prunePermissionRefusals(items);
    // Content guard: the pending set, the displayed refusals and the card
    // visibility are the entire render input. An unchanged delta (the common
    // streaming case has no new approval) keeps the card, its focus and its
    // open details exactly as they are.
    var guardKey =
      hash128(jsonForHash(items)) + '#' + hash128(jsonForHash(permissionRefusals));
    if (renderedPermissionsKey !== null && renderedPermissionsKey === guardKey) {
      return;
    }
    renderedPermissionsKey = guardKey;
    renderProbe('permissions');
    var focus = captureFocus(list);
    clear(list);
    // Bounded aria-live: a request id that was not pending in the previous
    // render is announced ONCE with its headline; an unchanged set never
    // re-announces, and focus is never moved.
    var pendingIds = [];
    for (var p = 0; p < items.length; p++) {
      pendingIds.push(String(items[p].id == null ? '' : items[p].id));
    }
    var announcements = [];
    if (lastPermissionIds !== null) {
      for (var q = 0; q < items.length; q++) {
        if (!lastPermissionIds.has(pendingIds[q])) {
          announcements.push(permissionHeadline(items[q]).headline);
        }
      }
    }
    lastPermissionIds = new Set(pendingIds);
    var announceNode = byId('permission-announce');
    if (announceNode) {
      if (announcements.length > 0) {
        announceNode.textContent =
          'Faktor needs your approval: ' +
          announcements[0] +
          (announcements.length > 1 ? ' (+' + (announcements.length - 1) + ' more)' : '');
      } else if (items.length === 0) {
        announceNode.textContent = '';
      }
    }
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
        var whyBits = [
          'capability ' + (capability.length > 0 ? capability : 'unavailable'),
          info.target !== null ? 'target ' + info.target : null,
          'session ' + (sessionId.length > 0 ? sessionId : 'unavailable'),
        ].filter(function (bit) {
          return bit !== null;
        });
        line(
          item,
          info.reason !== null
            ? 'Requester-stated reason (not verified): ' + info.reason
            : 'Action requested for this run. \u00b7 ' + whyBits.join(' \u00b7 '),
          'muted permission-why',
        );
        var refusal = permissionRefusalFor(id);
        if (refusal !== null) {
          line(item, refusal, 'muted permission-refusal');
        }
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
   * The empty state is ONE compact line. It exists only while the
   * conversation is empty: the first rendered entry removes it for good.
   * There is no welcome wall, card or suggestion list above the transcript.
   */
  function renderWelcome(entries) {
    var node = byId('welcome');
    if (!node) {
      return;
    }
    var empty = !entries || entries.length === 0;
    node.hidden = !empty;
  }

  var GOAL_MIN_HEIGHT = 36;
  var GOAL_MAX_HEIGHT = 120;

  /**
   * ONE auto-growing input: measure the scroll height, clamp it to 36-120px
   * and let the textarea scroll only after the cap. Called on every input and
   * after a programmatic draft update; the CSS max-height is the hard bound.
   */
  function autoGrowGoal() {
    var node = byId('goal');
    if (!node || !node.style) {
      return;
    }
    node.style.height = 'auto';
    var measured = typeof node.scrollHeight === 'number' ? node.scrollHeight : 0;
    if (!(measured > 0)) {
      measured = GOAL_MIN_HEIGHT;
    }
    var height = Math.max(GOAL_MIN_HEIGHT, Math.min(GOAL_MAX_HEIGHT, measured));
    node.style.height = height + 'px';
    node.style.overflowY = height >= GOAL_MAX_HEIGHT ? 'auto' : 'hidden';
  }

  /**
   * The daemon's typed configuration blocker recorded on a requested PR
   * completion step (`crates/orchestrator/src/completion_steps.rs:
   * NATIVE_PR_SCM_NOT_CONFIGURED`). It is the ONLY signal the T22 surface
   * accepts: no other PR failure is read as a missing-SCM configuration,
   * and PR success is never inferred.
   */
  var NATIVE_PR_SCM_NOT_CONFIGURED = 'native_pr_scm_not_configured';

  /** True when the requested PR step is blocked by the typed SCM code. */
  function prScmSetupBlocked(completion) {
    var steps = completion && Array.isArray(completion.steps) ? completion.steps : [];
    for (var i = 0; i < steps.length; i++) {
      var step = steps[i] || {};
      var kind = String(step.step == null ? '' : step.step).toLowerCase();
      var detail = String(step.detail == null ? '' : step.detail);
      if (kind === 'pr' && detail.indexOf(NATIVE_PR_SCM_NOT_CONFIGURED) !== -1) {
        return true;
      }
    }
    return false;
  }

  /** The T22 head of a requested PR finish step blocked by missing SCM config. */
  function prScmSetupNode() {
    var head = document.createElement('div');
    head.className = 'completion-head warn pr-scm-setup-title';
    head.textContent = 'Pull request integration unavailable';
    var copy = document.createElement('div');
    copy.className = 'muted pr-scm-setup-copy';
    copy.textContent = 'Configure the native SCM integration; no PR success is inferred.';
    var actions = document.createElement('div');
    actions.className = 'composer-actions';
    var button = document.createElement('button');
    button.type = 'button';
    button.textContent = 'Open setup guidance';
    button.addEventListener('click', function () {
      openView('diagnostics');
      setSandboxGuidanceOpen(true);
    });
    actions.appendChild(button);
    var node = document.createElement('div');
    node.className = 'pr-scm-setup';
    node.appendChild(head);
    node.appendChild(copy);
    node.appendChild(actions);
    return node;
  }

  /**
   * The durable completion-contract block. The step rows are exactly what
   * the host reports: `daemon` rows when the serving daemon exposes the
   * read, `derived` rows projected from the durable task-run state, or an
   * explicit `unavailable` (never fabricated as success). A requested PR
   * step blocked by the daemon's typed SCM-not-configured code carries the
   * T22 notice with the exact copy and the Diagnostics guidance action.
   */
  function renderCompletion(task) {
    var node = byId('task-completion');
    if (!node) {
      return;
    }
    // Completion is Verification material, never part of the Overview
    // summary. The old duplicated contract wall is gone from Work/Overview.
    node.hidden = activeSubview !== 'verification';
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
    if (prScmSetupBlocked(completion)) {
      node.appendChild(prScmSetupNode());
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
    if (!card || !list) {
      return;
    }
    // Same content-key guard as the changes/history lists: the host re-posts
    // the WHOLE snapshot per stream delta, so an unchanged agent set must
    // not tear down and re-create up to MAX_AGENT_ENTRIES rows (the renderer
    // is a pure function of `agents` and the active subview).
    var guardKey =
      activeSubview +
      '#' +
      (Array.isArray(agents) && agents.length > 0 ? hash128(jsonForHash(agents)) : 'none');
    if (renderedAgentsKey !== null && renderedAgentsKey === guardKey) {
      return;
    }
    renderedAgentsKey = guardKey;
    var focus = captureFocus(list);
    clear(list);
    // The agents surface belongs to the Inspect / Agents subview: another
    // subview never shows it, and no agent is ever fabricated.
    if (activeSubview !== 'agents' || !agents || agents.length === 0) {
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

  /** Structural identity: cheap lengths/counts used to reuse content keys. */
  function transcriptEntryStructure(entry) {
    var tools = entry.tools || [];
    var out =
      entry.id +
      ':' +
      entry.seq +
      ':' +
      entry.role +
      ':' +
      String(entry.text == null ? '' : entry.text).length +
      ':' +
      String(entry.reasoning == null ? '' : entry.reasoning).length +
      ':' +
      String(entry.summary == null ? '' : entry.summary).length +
      ':';
    for (var t = 0; t < tools.length; t++) {
      out +=
        String(tools[t].state) +
        ':' +
        String(tools[t].name == null ? '' : tools[t].name).length +
        ':' +
        String(tools[t].excerpt == null ? '' : tools[t].excerpt).length +
        ':' +
        String(tools[t].artifact == null ? '' : tools[t].artifact) +
        ':' +
        (tools[t].exitCode === null || tools[t].exitCode === undefined
          ? 'n'
          : tools[t].exitCode) +
        ';';
    }
    return out;
  }

  function transcriptStructuresOf(entries) {
    var out = [];
    for (var i = 0; i < entries.length; i++) {
      out.push(transcriptEntryStructure(entries[i]));
    }
    return out;
  }

  /**
   * Per-entry content keys. The host supplies `transcriptRevision` (a content
   * digest of the transcript it posts): when it is unchanged the transcript
   * is skipped entirely, and when it changed only entries whose structural
   * identity moved are hashed. A revision change with an unchanged structure
   * (a same-length content edit) forces one full re-key so a hash, never a
   * length, decides what reaches the DOM.
   */
  function transcriptKeysFor(entries, revisionChanged, forceFullHash) {
    var structures = transcriptStructuresOf(entries);
    if (forceFullHash) {
      // No host content digest (an older/legacy snapshot): hash every entry so
      // a same-length edit can never be mistaken for an unchanged row.
      return { keys: transcriptKeysOf(entries), structures: structures };
    }
    var reusable =
      transcriptEntries !== null &&
      transcriptEntryKeys !== null &&
      transcriptEntryStructures !== null &&
      entries.length === transcriptEntryStructures.length;
    var keys = [];
    var hashed = 0;
    for (var i = 0; i < entries.length; i++) {
      if (reusable && structures[i] === transcriptEntryStructures[i]) {
        keys.push(transcriptEntryKeys[i]);
      } else {
        keys.push(transcriptEntryKey(entries[i]));
        hashed += 1;
      }
    }
    if (revisionChanged && hashed === 0 && entries.length > 0) {
      keys = transcriptKeysOf(entries);
    }
    return { keys: keys, structures: structures };
  }

  /**
   * Documented-content equality for a revision that claims "nothing moved".
   * A hash is not an authority: a collision (or a stale/forged revision)
   * must never hide different content. The compare is O(entries) string
   * equality over the same fields the keys hash; it does no hashing, layout
   * or DOM work, so the idle no-op path stays cheap while remaining exact.
   */
  function transcriptContentSame(left, right) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
      return false;
    }
    for (var index = 0; index < left.length; index += 1) {
      var a = left[index];
      var b = right[index];
      if (!a || !b) {
        return false;
      }
      if (a.id !== b.id || a.seq !== b.seq || a.role !== b.role) {
        return false;
      }
      if (
        String(a.text == null ? '' : a.text) !== String(b.text == null ? '' : b.text) ||
        String(a.reasoning == null ? '' : a.reasoning) !==
          String(b.reasoning == null ? '' : b.reasoning) ||
        String(a.summary == null ? '' : a.summary) !==
          String(b.summary == null ? '' : b.summary)
      ) {
        return false;
      }
      var leftTools = a.tools || [];
      var rightTools = b.tools || [];
      if (leftTools.length !== rightTools.length) {
        return false;
      }
      for (var tool = 0; tool < leftTools.length; tool += 1) {
        if (!toolContentSame(leftTools[tool], rightTools[tool])) {
          return false;
        }
      }
    }
    return true;
  }

  function sameTranscriptKeys(left, right) {
    if (left === null || right === null || left.length !== right.length) {
      return false;
    }
    for (var i = 0; i < left.length; i++) {
      if (left[i] !== right[i]) {
        return false;
      }
    }
    return true;
  }

  function visibleTranscriptKeysSame(previous, next, start, end) {
    if (previous === null || next.length < previous.length || end > previous.length) {
      return false;
    }
    for (var index = start; index < end; index += 1) {
      if (previous[index] !== next[index]) {
        return false;
      }
    }
    return true;
  }

  function prefixTranscriptKeys(previous, next) {
    if (previous === null || previous.length === 0 || next.length <= previous.length) {
      return false;
    }
    for (var i = 0; i < previous.length; i++) {
      if (previous[i] !== next[i]) {
        return false;
      }
    }
    return true;
  }

  /** Capture focus inside the transcript (entry index + structural path). */
  function captureTranscriptFocus(container) {
    var active = typeof document !== 'undefined' ? document.activeElement : null;
    if (!active || !container || !container.contains || !container.contains(active)) {
      return null;
    }
    var index = -1;
    var cursor = active;
    while (cursor && cursor !== container) {
      var attribute = cursor.getAttribute ? cursor.getAttribute('data-entry-index') : null;
      if (attribute !== null && attribute !== undefined) {
        index = Number(attribute);
        break;
      }
      cursor = cursor.parentNode;
    }
    return {
      index: index,
      path: index >= 0 ? elementPath(container, active) : null,
      tag: active.tagName,
      text: active.textContent,
      aria: active.getAttribute ? active.getAttribute('aria-label') : null,
    };
  }

  function restoreTranscriptFocus(container, descriptor) {
    if (!descriptor) {
      return;
    }
    if (descriptor.path) {
      var exact = elementAtPath(container, descriptor.path);
      if (exact && exact.tagName === descriptor.tag && typeof exact.focus === 'function') {
        exact.focus();
        return;
      }
    }
    var candidates = container.querySelectorAll
      ? container.querySelectorAll('button, input, textarea, select, a, [tabindex]')
      : [];
    for (var i = 0; i < candidates.length; i++) {
      var node = candidates[i];
      var match =
        descriptor.aria !== null && descriptor.aria !== undefined
          ? node.getAttribute && node.getAttribute('aria-label') === descriptor.aria
          : node.textContent === descriptor.text;
      if (match && node.tagName === descriptor.tag) {
        if (typeof node.focus === 'function') {
          node.focus();
        }
        return;
      }
    }
    // The focused entry scrolled out of the window: keep keyboard users in
    // the transcript instead of dropping focus onto the document body.
    if (typeof container.focus === 'function') {
      container.focus();
    }
  }

  /** The first visible entry, via a binary search over the cached heights. */
  function transcriptAnchorFromScroll(prefix, scrollTop) {
    var lo = 0;
    var hi = Math.max(0, prefix.length - 2);
    while (lo < hi) {
      var mid = (lo + hi + 1) >> 1;
      if (prefix[mid] <= scrollTop) {
        lo = mid;
      } else {
        hi = mid - 1;
      }
    }
    return { index: lo, offset: Math.max(0, scrollTop - prefix[lo]) };
  }

  /** The first visible entry of the short (non-windowed) list. */
  function transcriptAnchorOf(container) {
    var offset = container.scrollTop;
    var children = container.children || [];
    var lo = 0;
    var hi = Math.max(0, children.length - 1);
    while (lo < hi) {
      var mid = (lo + hi + 1) >> 1;
      var midTop = typeof children[mid].offsetTop === 'number' ? children[mid].offsetTop : 0;
      if (midTop <= offset) {
        lo = mid;
      } else {
        hi = mid - 1;
      }
    }
    var node = children[lo];
    var top = node && typeof node.offsetTop === 'number' ? node.offsetTop : 0;
    return { index: lo, offset: Math.max(0, offset - top) };
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
    setTranscriptScroll(container, top + anchor.offset);
  }

  /**
   * Programmatic scrolls are followed by a scroll event in a real browser.
   * The window updater is a no-op when the current window already covers the
   * position, so this flag only avoids redundant work inside the same task.
   */
  function setTranscriptScroll(container, top) {
    transcriptSuppressScroll = true;
    container.scrollTop = top;
    // The browser may clamp to the real maximum; the written value is still
    // the intent. Its own scroll event is ignored unless a user gesture is
    // active, and a gesture inside the pin slack resolves back to following.
    transcriptLastWrittenTop = top;
    transcriptSuppressScroll = false;
  }

  function transcriptHeightAt(index) {
    var height = transcriptHeights[index];
    return typeof height === 'number' && height > 0 ? height : TRANSCRIPT_ESTIMATED_ENTRY_PX;
  }

  /** Prefix offsets of every entry (height + flex gap), used for windowing. */
  function transcriptPrefixOf(entries) {
    var prefix = [0];
    for (var i = 0; i < entries.length; i++) {
      prefix.push(prefix[i] + transcriptHeightAt(i) + TRANSCRIPT_ENTRY_GAP_PX);
    }
    return prefix;
  }

  function transcriptRows(container) {
    var client = container.clientHeight;
    if (!(typeof client === 'number' && client > 0)) {
      client = 600;
    }
    var visible = Math.ceil(client / (TRANSCRIPT_ESTIMATED_ENTRY_PX + TRANSCRIPT_ENTRY_GAP_PX));
    return Math.max(TRANSCRIPT_WINDOW_MIN_ROWS, visible + TRANSCRIPT_WINDOW_OVERSCAN);
  }

  function transcriptSpacer(className, height) {
    var node = document.createElement('div');
    node.className = 'transcript-spacer ' + className;
    node.setAttribute('aria-hidden', 'true');
    setSpacerHeight(node, height);
    return node;
  }

  function setSpacerHeight(node, height) {
    var value = Math.max(0, Math.round(height));
    if (node && node.style && typeof node.style === 'object') {
      node.style.height = value + 'px';
    } else if (node && node.setAttribute) {
      node.setAttribute('data-height', String(value));
    }
  }

  /** Measure only the rendered window and cache the heights by entry index. */
  function measureTranscriptHeights(container) {
    var children = container.children || [];
    for (var i = 0; i < children.length; i++) {
      var node = children[i];
      var attribute = node.getAttribute ? node.getAttribute('data-entry-index') : null;
      if (attribute === null || attribute === undefined) {
        continue;
      }
      var index = Number(attribute);
      if (!isFinite(index) || index < 0) {
        continue;
      }
      var height = typeof node.offsetHeight === 'number' ? node.offsetHeight : 0;
      if (height > 0) {
        transcriptHeights[index] = height;
      }
    }
  }

  /**
   * Suppress the live region while a structural rebuild replaces entry nodes:
   * only genuinely appended messages should be announced, never the preserved
   * history a window shift re-materializes.
   */
  var transcriptLiveSuppressDepth = 0;
  var transcriptLivePrevious = null;
  var transcriptLiveRestoreToken = 0;

  function withTranscriptLiveOff(container, mutate) {
    if (transcriptLiveSuppressDepth === 0) {
      if (transcriptLivePrevious === null) {
        var current = container.getAttribute ? container.getAttribute('aria-live') : null;
        transcriptLivePrevious = current && current !== 'off' ? current : 'polite';
      }
      if (container.setAttribute) {
        container.setAttribute('aria-live', 'off');
      }
    }
    transcriptLiveSuppressDepth += 1;
    try {
      mutate();
    } finally {
      transcriptLiveSuppressDepth -= 1;
      if (transcriptLiveSuppressDepth === 0) {
        var token = ++transcriptLiveRestoreToken;
        var restore = function () {
          if (token !== transcriptLiveRestoreToken || transcriptLiveSuppressDepth !== 0) {
            return;
          }
          if (container.setAttribute) {
            container.setAttribute('aria-live', transcriptLivePrevious || 'polite');
          }
          transcriptLivePrevious = null;
        };
        if (typeof Promise !== 'undefined' && typeof Promise.resolve === 'function') {
          Promise.resolve().then(restore);
        } else {
          restore();
        }
      }
    }
  }

  /** True when only the last entry's content moved (streaming fast path). */
  function lastEntryOnlyChanged(keys, previous) {
    if (previous === null || previous.length !== keys.length || keys.length === 0) {
      return false;
    }
    if (previous[previous.length - 1] === keys[keys.length - 1]) {
      return false;
    }
    for (var i = 0; i < keys.length - 1; i++) {
      if (previous[i] !== keys[i]) {
        return false;
      }
    }
    return true;
  }

  /**
   * Update a text part WITHOUT replacing its text node. `textContent = ...`
   * detaches the old text node, which collapses a user's text selection and
   * can drop focus while a delta streams; writing the existing node's data
   * keeps the node identity (and therefore the selection offsets) intact.
   * The fallback covers an element whose content is not exactly one text
   * node (structured/legacy rows): there is no selection to preserve there.
   */
  /**
   * Capture a text selection that lies ENTIRELY inside `textNode`.
   * Any mutation of the node's character data collapses such a selection in
   * Blink (even `nodeValue = sameLengthText`), so the offsets are restored
   * after the patch. A selection spanning several nodes is left alone (this
   * patch never removes the selected nodes).
   */
  function captureTextSelection(textNode) {
    if (typeof window === 'undefined' || typeof window.getSelection !== 'function') {
      return null;
    }
    var selection = window.getSelection();
    if (!selection || selection.rangeCount === 0) {
      return null;
    }
    var range = selection.getRangeAt(0);
    if (range.startContainer !== textNode || range.endContainer !== textNode) {
      return null;
    }
    var backward = false;
    try {
      backward =
        selection.anchorNode === textNode &&
        selection.focusNode === textNode &&
        selection.anchorOffset > selection.focusOffset;
    } catch (error) {
      backward = false;
    }
    return { start: range.startOffset, end: range.endOffset, backward: backward };
  }

  function restoreTextSelection(textNode, saved) {
    if (!saved || !textNode || textNode.nodeType !== 3) {
      return;
    }
    if (typeof window === 'undefined' || typeof window.getSelection !== 'function') {
      return;
    }
    var selection = window.getSelection();
    if (!selection) {
      return;
    }
    var max = textNode.nodeValue == null ? 0 : textNode.nodeValue.length;
    var start = Math.min(Math.max(0, saved.start), max);
    var end = Math.min(Math.max(0, saved.end), max);
    try {
      if (typeof selection.setBaseAndExtent === 'function') {
        selection.setBaseAndExtent(
          textNode,
          saved.backward ? end : start,
          textNode,
          saved.backward ? start : end,
        );
      } else {
        var range = document.createRange();
        range.setStart(textNode, start);
        range.setEnd(textNode, end);
        selection.removeAllRanges();
        selection.addRange(range);
      }
    } catch (error) {
      // A selection API without this node (detached) simply stays as-is.
    }
  }

  function setPartText(part, value) {
    if (!part) {
      return;
    }
    var next = value == null ? '' : String(value);
    var first = part.firstChild;
    if (
      first &&
      first.nodeType === 3 &&
      part.childNodes &&
      part.childNodes.length === 1
    ) {
      if (first.nodeValue === next) {
        return;
      }
      var selection = captureTextSelection(first);
      first.nodeValue = next;
      restoreTextSelection(first, selection);
      return;
    }
    if (part.textContent !== next) {
      part.textContent = next;
    }
  }

  function toolContentSame(left, right) {
    if (left === right) {
      return true;
    }
    if (!left || !right) {
      return false;
    }
    return (
      left.state === right.state &&
      left.name === right.name &&
      left.excerpt === right.excerpt &&
      left.artifact === right.artifact &&
      left.exitCode === right.exitCode
    );
  }

  /**
   * Incremental update of one already-rendered entry: text/reasoning/summary
   * are patched in place and only tool rows whose content moved are replaced.
   * The article itself (and focus inside untouched descendants) survives the
   * per-token delta.
   */
  function patchEntryNode(node, entry) {
    var old = node ? node.__faktorEntry : null;
    var parts = node ? node.__faktorParts : null;
    if (!old || !parts || old.seq !== entry.seq || old.role !== entry.role) {
      return false;
    }
    var oldTools = old.tools || [];
    var newTools = entry.tools || [];
    if (oldTools.length !== newTools.length) {
      return false;
    }
    if (!!entry.text !== !!(parts.text && parts.text.__faktorPresent)) {
      return false;
    }
    if (!!entry.reasoning !== !!(parts.reasoning && parts.reasoning.__faktorPresent)) {
      return false;
    }
    if (!!entry.summary !== !!(parts.summary && parts.summary.__faktorPresent)) {
      return false;
    }
    var focusInside =
      typeof document !== 'undefined' &&
      document.activeElement &&
      node.contains &&
      node.contains(document.activeElement);
    var preserved = focusInside ? captureEntryState(node) : null;
    setPartText(parts.text, entry.text);
    setPartText(parts.reasoning, entry.reasoning);
    setPartText(parts.summary, entry.summary);
    var toolNodes = node.__faktorToolNodes || [];
    for (var t = 0; t < newTools.length; t += 1) {
      var oldTool = oldTools[t];
      var newTool = newTools[t];
      if (toolContentSame(oldTool, newTool)) {
        continue;
      }
      var existingRow = toolNodes[t];
      // Streamed tool output (same row identity, growing excerpt) is patched
      // in place: the disclosure, focus and evidence controls survive.
      var canPatchTool =
        existingRow &&
        node.contains &&
        node.contains(existingRow) &&
        oldTool &&
        oldTool.state === newTool.state &&
        oldTool.name === newTool.name &&
        oldTool.artifact === newTool.artifact &&
        oldTool.exitCode === newTool.exitCode;
      if (canPatchTool) {
        var oldExcerpt = oldTool.excerpt == null ? '' : String(oldTool.excerpt);
        var newExcerpt = newTool.excerpt == null ? '' : String(newTool.excerpt);
        if (existingRow.__faktorExcerpt) {
          setPartText(existingRow.__faktorExcerpt, newExcerpt);
        } else if (newExcerpt.length > 0) {
          canPatchTool = false;
        }
      }
      if (canPatchTool) {
        var nextCounts =
          existingRow.__faktorToolKind === 'edit' ? diffCounts(newTool.excerpt) : null;
        if (nextCounts !== null) {
          if (existingRow.__faktorDiffAdd && existingRow.__faktorDiffDel) {
            var addText = '+' + nextCounts.adds;
            var delText = '\u2212' + nextCounts.dels;
            if (existingRow.__faktorDiffAdd.textContent !== addText) {
              existingRow.__faktorDiffAdd.textContent = addText;
            }
            if (existingRow.__faktorDiffDel.textContent !== delText) {
              existingRow.__faktorDiffDel.textContent = delText;
            }
          } else {
            canPatchTool = false;
          }
        } else if (existingRow.__faktorDiffAdd || existingRow.__faktorDiffDel) {
          // The excerpt is no longer diff-shaped: replace the row so the
          // stale +N/−N chip cannot survive a streamed edit.
          canPatchTool = false;
        }
      }
      if (canPatchTool) {
        existingRow.__faktorTool = newTool;
        continue;
      }
      // Preserve the OLD row's open disclosure / delivered evidence even when
      // focus has left it (the reader may have opened a tool card and then
      // clicked into the composer). The capture is scoped to this row, so a
      // per-token text patch never clones the whole entry.
      var rowState = existingRow ? captureEntryState(existingRow) : null;
      var fresh = renderToolNode(newTool);
      if (existingRow && existingRow.parentNode && (!node.contains || node.contains(existingRow))) {
        existingRow.parentNode.replaceChild(fresh, existingRow);
      } else {
        var host = node.__faktorToolGroup
          ? node.querySelector('.activity-group-body')
          : node;
        if (host) host.appendChild(fresh);
      }
      if (rowState) {
        restoreEntryState(fresh, rowState);
      }
      toolNodes[t] = fresh;
    }
    if (node.__faktorToolGroup) {
      updateToolGroupSummary(node.__faktorToolGroup, newTools);
    }
    node.__faktorToolNodes = toolNodes;
    node.__faktorEntry = entry;
    if (preserved) {
      restoreEntryState(node, preserved);
    }
    return true;
  }

  /** Update only the last rendered entry (windowed or short list). */
  function patchLastTranscriptEntry(container, entries, keys, pin, anchor, index) {
    var children = container.children || [];
    var node = children[children.length - 1];

    if (!node || !node.getAttribute || Number(node.getAttribute('data-entry-index')) !== index) {
      return false;
    }
    renderProbe('transcript-last');
    var entry = entries[index];
    var patched = patchEntryNode(node, entry);
    renderProbe(patched ? 'transcript-patch' : 'transcript-patch-fallback');
    if (!patched) {
      var preserved = captureEntryState(node);
      var fresh = renderEntry(entry);
      fresh.setAttribute('data-entry-index', String(index));
      container.replaceChild(fresh, node);
      restoreEntryState(fresh, preserved);
      node = fresh;
    }
    // No offsetHeight read here: a per-token measurement would force a
    // synchronous layout on every delta. The visible-window render measures
    // heights once per window change; the pinned follow uses the browser's
    // own scroll height.
    transcriptPrefix = transcriptPrefixOf(entries);
    transcriptEntries = entries;
    transcriptEntryKeys = keys;
    transcriptEntryStructures = transcriptStructuresOf(entries);
    transcriptKey = entries.length === 0 ? 'empty' : 'entries';
    transcriptPinRequested = false;
    transcriptPinned = pin;
    transcriptAnchor = pin ? null : anchor;
    if (pin) {
      setTranscriptScroll(container, container.scrollHeight);
    } else if (anchor !== null) {
      setTranscriptScroll(
        container,
        transcriptPrefix[anchor.index] + anchor.offset,
      );
    }
    return true;
  }

  /**
   * Materialize one visible window with top/bottom spacers that reproduce the
   * cached full height, so scroll position and total scroll range keep their
   * meaning. Only window changes touch the DOM; a per-token delta goes through
   * patchLastTranscriptEntry.
   */
  function renderTranscriptWindow(container, entries, keys, pin, anchor) {
    var total = entries.length;
    transcriptPrefix = transcriptPrefixOf(entries);
    var rows = transcriptRows(container);
    var start;
    var end;
    if (pin) {
      end = total;
      start = Math.max(0, total - rows);
    } else {
      var anchorIndex = anchor === null ? 0 : anchor.index;
      start = Math.max(0, Math.min(anchorIndex - Math.floor(rows / 2), Math.max(0, total - rows)));
      end = Math.min(total, start + rows);
    }
    var windowSame =
      transcriptWindowed &&
      start === transcriptWindowStart &&
      end === transcriptWindowEnd;
    if (windowSame && sameTranscriptKeys(keys, transcriptEntryKeys) && entries.length === transcriptEntries.length) {
      transcriptEntries = entries;
      transcriptEntryStructures = transcriptStructuresOf(entries);
      transcriptKey = entries.length === 0 ? 'empty' : 'entries';
      transcriptPinRequested = false;
      transcriptPinned = pin;
      transcriptAnchor = pin ? null : anchor;
      if (pin) {
        setTranscriptScroll(container, container.scrollHeight);
      } else if (anchor !== null) {
        var prefix = transcriptPrefix;
        setTranscriptScroll(container, prefix[anchor.index] + anchor.offset);
      }
      return;
    }
    // While the reader is away from the bottom, a delta on an entry OUTSIDE
    // the visible window must not re-materialize it: only the cached total
    // height (and therefore the bottom spacer) changes.
    if (
      windowSame &&
      !pin &&
      entries.length >= transcriptEntries.length &&
      end <= transcriptEntries.length &&
      visibleTranscriptKeysSame(transcriptEntryKeys, keys, start, end)
    ) {
      renderProbe('transcript-window-offscreen');
      var offscreenBottom = container.children[container.children.length - 1];
      if (
        offscreenBottom &&
        offscreenBottom.className &&
        String(offscreenBottom.className).indexOf('transcript-spacer-bottom') !== -1
      ) {
        setSpacerHeight(
          offscreenBottom,
          transcriptPrefix[total] - transcriptPrefix[end] - TRANSCRIPT_ENTRY_GAP_PX,
        );
      }
      transcriptEntries = entries;
      transcriptEntryKeys = keys;
      transcriptEntryStructures = transcriptStructuresOf(entries);
      transcriptKey = entries.length === 0 ? 'empty' : 'entries';
      transcriptPinRequested = false;
      transcriptAnchor = anchor;
      return;
    }
    var focus = captureTranscriptFocus(container);
    renderProbe(pin ? 'transcript-window-pinned' : 'transcript-window-anchored');
    withTranscriptLiveOff(container, function () {
      clear(container);
      var fragment =
        typeof document.createDocumentFragment === 'function' ? document.createDocumentFragment() : null;
      var target = fragment || container;
      if (start > 0) {
        target.appendChild(transcriptSpacer('transcript-spacer-top', transcriptPrefix[start] - TRANSCRIPT_ENTRY_GAP_PX));
      }
      for (var i = start; i < end; i += 1) {
        var node = renderEntry(entries[i]);
        node.setAttribute('data-entry-index', String(i));
        target.appendChild(node);
      }
      if (end < total) {
        target.appendChild(
          transcriptSpacer(
            'transcript-spacer-bottom',
            transcriptPrefix[total] - transcriptPrefix[end] - TRANSCRIPT_ENTRY_GAP_PX,
          ),
        );
      }
      if (fragment) {
        container.appendChild(fragment);
      }
    });
    measureTranscriptHeights(container);
    transcriptPrefix = transcriptPrefixOf(entries);
    var top = container.children[0];
    if (top && top.className && String(top.className).indexOf('transcript-spacer-top') !== -1) {
      setSpacerHeight(top, transcriptPrefix[start] - TRANSCRIPT_ENTRY_GAP_PX);
    }
    var bottom = container.children[container.children.length - 1];
    if (bottom && bottom.className && String(bottom.className).indexOf('transcript-spacer-bottom') !== -1) {
      setSpacerHeight(
        bottom,
        transcriptPrefix[total] - transcriptPrefix[end] - TRANSCRIPT_ENTRY_GAP_PX,
      );
    }
    transcriptWindowed = true;
    transcriptWindowStart = start;
    transcriptWindowEnd = end;
    transcriptEntries = entries;
    transcriptEntryKeys = keys;
    transcriptEntryStructures = transcriptStructuresOf(entries);
    transcriptKey = entries.length === 0 ? 'empty' : 'entries';
    transcriptPinRequested = false;
    transcriptPinned = pin;
    transcriptAnchor = pin ? null : anchor;
    if (pin) {
      setTranscriptScroll(container, container.scrollHeight);
    } else if (anchor !== null) {
      setTranscriptScroll(container, transcriptPrefix[anchor.index] + anchor.offset);
    }
    restoreTranscriptFocus(container, focus);
  }

  function onTranscriptScroll() {
    if (transcriptEntries === null || transcriptSuppressScroll) {
      return;
    }
    var container = byId('entries');
    // Only a recent input gesture makes this scroll user intent. A bare
    // scroll event (programmatic write, browser anchoring, restoration) is
    // ignored and can never flip the pin.
    if (Date.now() > transcriptUserScrollUntil) {
      return;
    }
    // A scroll event at the exact position this renderer wrote is its own
    // follow, even inside the gesture window (e.g. the gesture settled at the
    // bottom just before the delta): it must not unpin.
    if (
      transcriptPinned &&
      Math.abs(container.scrollTop - transcriptLastWrittenTop) <= 1
    ) {
      renderProbe('scroll-programmatic');
      return;
    }
    // Record the user's intent IMMEDIATELY (before any RAF): a snapshot that
    // arrives in the same task must already see the new pin/anchor and never
    // scroll the reader back to the bottom first.
    var plan = transcriptScrollPlan({
      scrollHeight: container.scrollHeight,
      scrollTop: container.scrollTop,
      clientHeight: container.clientHeight,
    });
    renderProbe(plan.pinned ? 'scroll-external-pinned' : 'scroll-external-anchored');
    transcriptPinned = plan.pinned;
    transcriptAnchor = plan.pinned
      ? null
      : transcriptWindowed
        ? transcriptAnchorFromScroll(transcriptPrefix, container.scrollTop)
        : transcriptAnchorOf(container);
    if (!transcriptWindowed || transcriptScrollScheduled) {
      return;
    }
    transcriptScrollScheduled = true;
    var run = function () {
      transcriptScrollScheduled = false;
      if (transcriptEntries === null) {
        return;
      }
      renderTranscriptWindow(
        container,
        transcriptEntries,
        transcriptEntryKeys,
        transcriptPinned,
        transcriptAnchor,
      );
    };
    if (typeof window.requestAnimationFrame === 'function') {
      window.requestAnimationFrame(run);
    } else {
      setTimeout(run, 16);
    }
  }

  function noteTranscriptUserScroll() {
    transcriptUserScrollUntil = Date.now() + 300;
  }

  function ensureTranscriptScrollListener(container) {
    if (transcriptScrollListenerAttached || !container.addEventListener) {
      return;
    }
    transcriptScrollListenerAttached = true;
    container.addEventListener('scroll', onTranscriptScroll, { passive: true });
    container.addEventListener('wheel', noteTranscriptUserScroll, { passive: true });
    container.addEventListener('touchstart', noteTranscriptUserScroll, { passive: true });
    container.addEventListener('touchmove', noteTranscriptUserScroll, { passive: true });
    container.addEventListener('pointerdown', noteTranscriptUserScroll, { passive: true });
    container.addEventListener('mousedown', noteTranscriptUserScroll, { passive: true });
    container.addEventListener('keydown', function (event) {
      var key = event && event.key ? String(event.key) : '';
      if (
        key === 'PageUp' ||
        key === 'PageDown' ||
        key === 'ArrowUp' ||
        key === 'ArrowDown' ||
        key === 'Home' ||
        key === 'End' ||
        key === ' '
      ) {
        noteTranscriptUserScroll();
      }
    });
  }

  function renderTranscript(entries, revision) {
    var container = byId('entries');
    entries = Array.isArray(entries) ? entries : [];
    ensureTranscriptScrollListener(container);
    var nextRevision =
      typeof revision === 'string' && revision.length > 0 ? revision : null;
    var revisionUnchanged =
      nextRevision !== null && transcriptRevision !== null && nextRevision === transcriptRevision;
    var digestCollision = false;
    if (revisionUnchanged && transcriptKey !== null) {
      // The host's content digest says nothing moved. A digest is a hint,
      // never an authority: verify the actual document before skipping, so a
      // collision (or a stale/forged revision) cannot freeze different
      // content on screen. Equal content does no hashing, layout or DOM work.
      if (transcriptContentSame(transcriptEntries, entries)) {
        return;
      }
      digestCollision = true;
    }
    var revisionChanged =
      digestCollision ||
      (nextRevision !== null && transcriptRevision !== null && nextRevision !== transcriptRevision);
    var computed = transcriptKeysFor(
      entries,
      revisionChanged,
      nextRevision === null || digestCollision,
    );
    var keys = computed.keys;
    var previousKeys = transcriptEntryKeys;
    var windowed = entries.length > TRANSCRIPT_WINDOW_THRESHOLD;
    // Scroll intent is computed BEFORE any mutation so the streaming fast
    // path honors the same pinned-follows / unpinned-anchor contract.
    // The pin is explicit state, not a value re-derived from scrollTop: a
    // browser-internal adjustment can never silently switch a reader back to
    // follow mode. Only user gestures (handled below) change it.
    var pin =
      transcriptKey === null || transcriptPinRequested || transcriptPinned;
    renderProbe(pin ? 'transcript-plan-pinned' : 'transcript-plan-anchored');
    var anchor = pin ? null : transcriptAnchor;
    var lastOnlyChanged = lastEntryOnlyChanged(keys, previousKeys);
    if (
      lastOnlyChanged &&
      !windowed &&
      !transcriptWindowed &&
      container.children.length === entries.length
    ) {
      if (patchLastTranscriptEntry(container, entries, keys, pin, anchor, entries.length - 1)) {
        transcriptRevision = nextRevision;
        return;
      }
    }
    if (windowed) {
      if (
        lastOnlyChanged &&
        transcriptWindowed &&
        transcriptWindowEnd === entries.length &&
        container.children.length > 0
      ) {
        var lastIndex = entries.length - 1;
        if (patchLastTranscriptEntry(container, entries, keys, pin, anchor, lastIndex)) {
          transcriptRevision = nextRevision;
          return;
        }
      }
      renderTranscriptWindow(container, entries, keys, pin, anchor);
      transcriptRevision = nextRevision;
      return;
    }
    if (transcriptWindowed) {
      // Crossing back under the threshold: drop the spacers with a full
      // rebuild (the short-list path owns the DOM from here on).
      transcriptWindowed = false;
      transcriptWindowStart = 0;
      transcriptWindowEnd = 0;
    }
    if (transcriptKey !== null && sameTranscriptKeys(keys, previousKeys)) {
      transcriptEntries = entries;
      transcriptEntryStructures = computed.structures;
      transcriptRevision = nextRevision;
      return;
    }
    // A strict prefix extension (the common snapshot append) adds ONLY the
    // new entries: existing DOM nodes are never re-inserted, so the live
    // region announces additions instead of re-announcing the history.
    var appended =
      previousKeys !== null &&
      previousKeys.length > 0 &&
      prefixTranscriptKeys(previousKeys, keys) &&
      container.children.length > 0 &&
      entries.length >= container.children.length;
    if (appended) {
      renderProbe('transcript-append');
      for (var a = container.children.length; a < entries.length; a++) {
        var appendedNode = renderEntry(entries[a]);
        appendedNode.setAttribute('data-entry-index', String(a));
        container.appendChild(appendedNode);
      }
    } else {
      renderProbe('transcript');
      var focus = captureTranscriptFocus(container);
      withTranscriptLiveOff(container, function () {
        clear(container);
        if (entries.length === 0) {
          return;
        }
        for (var i = 0; i < entries.length; i++) {
          var node = renderEntry(entries[i]);
          node.setAttribute('data-entry-index', String(i));
          container.appendChild(node);
        }
      });
      restoreTranscriptFocus(container, focus);
    }
    transcriptKey = keys.length === 0 ? 'empty' : 'entries';
    transcriptEntryKeys = keys;
    transcriptEntryStructures = computed.structures;
    transcriptEntries = entries;
    transcriptRevision = nextRevision;
    transcriptPinRequested = false;
    transcriptPinned = pin;
    transcriptAnchor = pin ? null : anchor;
    if (entries.length === 0) {
      // The empty slot stays empty: the onboarding/welcome state right above
      // the log explains the panel; a duplicated "no messages" line would
      // only restate it.
      if (pin) {
        setTranscriptScroll(container, 0);
      }
      return;
    }
    if (pin) {
      setTranscriptScroll(container, container.scrollHeight);
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
    return cachedRender('diff', hash128(excerpt), excerpt, function () {
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
    });
  }

  /** The outcome tone of one tool from its exit code and durable state. */
  function toolOutcome(tool) {
    var excerpt = typeof tool.excerpt === 'string' ? tool.excerpt : '';
    // A denied call is written as a typed tool_result whose excerpt starts
    // with `tool call denied (<kind>): ...` and which carries NO exit code.
    // It is a refusal, never a success: without this branch the generic
    // "completed + non-empty excerpt" path painted it green as "done".
    if (excerpt.indexOf('tool call denied (') === 0) {
      return { tone: 'fail', label: 'denied' };
    }
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
    // `completed` on a tool_call part means the MODEL finished emitting the
    // call, not that the tool ran. Without a result (no exit code, no
    // excerpt) the call was denied/cancelled/still waiting; never paint it
    // green as "done".
    var hasResult =
      (typeof tool.excerpt === 'string' && tool.excerpt.length > 0) ||
      (tool.artifact !== null && tool.artifact !== undefined);
    if ((state === 'completed' || state === 'done') && !hasResult) {
      return { tone: 'neutral', label: 'requested' };
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

  function renderToolNode(tool) {
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
    var diffAdd = null;
    var diffDel = null;
    if (chip.tone === 'diff') {
      var diff = span(summary, '', 'activity-chip activity-chip-diff');
      diffAdd = span(diff, '+' + counts.adds, 'activity-add');
      diffDel = span(diff, '\u2212' + counts.dels, 'activity-del');
    } else {
      span(summary, chip.label, 'activity-chip activity-chip-' + chip.tone);
    }
    details.appendChild(summary);

    var excerptNode = null;
    if (tool.excerpt) {
      excerptNode = document.createElement('pre');
      excerptNode.className = 'excerpt';
      excerptNode.textContent = tool.excerpt;
      details.appendChild(excerptNode);
    }

    var evidenceId = evidenceIdOf(tool.artifact);
    if (evidenceId !== null) {
      // Evidence rows are a LIST: the region carries role=list and each
      // evidence row is a listitem with an accessible name; the retrieve
      // button stays the only control.
      var region = document.createElement('div');
      region.className = 'evidence-list';
      region.setAttribute('role', 'list');
      region.setAttribute('aria-label', 'Evidence artifacts');
      var holder = document.createElement('div');
      holder.className = 'evidence';
      holder.setAttribute('role', 'listitem');
      holder.setAttribute('aria-label', 'Evidence #' + evidenceId);
      holder.setAttribute('data-evidence', String(evidenceId));
      var button = document.createElement('button');
      button.type = 'button';
      button.textContent = 'View evidence #' + evidenceId;
      button.addEventListener('click', function () {
        vscode.postMessage({ type: 'retrieveEvidence', evidenceId: evidenceId });
      });
      holder.appendChild(button);
      region.appendChild(holder);
      details.appendChild(region);
    }
    row.appendChild(details);
    row.__faktorExcerpt = excerptNode;
    row.__faktorDiffAdd = diffAdd;
    row.__faktorDiffDel = diffDel;
    row.__faktorToolKind = kind;
    row.__faktorTool = tool;
    return row;
  }

  function textPart(wrapper, className, value) {
    var node = document.createElement('div');
    node.className = className;
    node.textContent = value;
    node.__faktorPresent = value !== null && value !== undefined && value !== '';
    wrapper.appendChild(node);
    return node;
  }

  function renderEntry(entry) {
    var wrapper = document.createElement('article');
    wrapper.className = 'entry entry-' + (entry.role === 'user' ? 'user' : 'assistant');

    var head = document.createElement('div');
    head.className = 'entry-head';
    head.textContent = entry.role + ' #' + entry.seq;
    wrapper.appendChild(head);

    var parts = { text: null, reasoning: null, summary: null };
    if (entry.text) {
      parts.text = textPart(wrapper, 'entry-text', entry.text);
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
      reasoning.__faktorPresent = true;
      details.appendChild(reasoning);
      wrapper.appendChild(details);
      parts.reasoning = reasoning;
    }
    if (entry.summary) {
      parts.summary = textPart(wrapper, 'entry-summary', entry.summary);
    }
    var tools = Array.isArray(entry.tools) ? entry.tools : [];
    var toolNodes = [];
    var toolHost = wrapper;
    var toolGroup = null;
    if (tools.length > 1) {
      toolGroup = document.createElement('details');
      toolGroup.className = 'activity-group';
      var groupSummary = document.createElement('summary');
      groupSummary.className = 'activity-group-summary';
      toolGroup.appendChild(groupSummary);
      toolHost = document.createElement('div');
      toolHost.className = 'activity-group-body';
      toolGroup.appendChild(toolHost);
      // A settled batch stays collapsed; live or failed work opens because
      // its output is the part worth reading.
      toolGroup.open = tools.some(function (tool) {
        var tone = toolOutcome(tool).tone;
        return tone === 'run' || tone === 'fail';
      });
      wrapper.appendChild(toolGroup);
    }
    for (var i = 0; i < tools.length; i++) {
      var toolNode = renderToolNode(tools[i]);
      toolHost.appendChild(toolNode);
      toolNodes.push(toolNode);
    }
    if (toolGroup !== null) {
      updateToolGroupSummary(toolGroup, tools);
    }
    wrapper.__faktorEntry = entry;
    wrapper.__faktorParts = parts;
    wrapper.__faktorToolNodes = toolNodes;
    wrapper.__faktorToolGroup = toolGroup;
    return wrapper;
  }

  /** One concise header for a batch of consecutive tool calls. */
  function updateToolGroupSummary(group, tools) {
    if (!group) {
      return;
    }
    var summary = group.querySelector('.activity-group-summary');
    if (!summary) {
      return;
    }
    var failed = 0;
    var running = 0;
    for (var i = 0; i < tools.length; i++) {
      var tone = toolOutcome(tools[i]).tone;
      if (tone === 'fail') failed += 1;
      else if (tone === 'run') running += 1;
    }
    var parts = [tools.length + (tools.length === 1 ? ' tool call' : ' tool calls')];
    if (failed > 0) parts.push(failed + ' failed');
    if (running > 0) parts.push(running + ' running');
    summary.textContent = parts.join(' · ');
    summary.setAttribute('aria-label', 'Tool activity: ' + parts.join(', '));
  }

  function setDaemon(status, detail, notice) {
    var dot = byId('daemon-dot');
    if (dot) {
      var dotClass = 'dot dot-' + (status || 'stopped');
      if (dot.className !== dotClass) {
        dot.className = dotClass;
      }
      // Healthy service state is invisible in the product surface. The dot
      // appears only with the two transient connection notices.
      dot.hidden = !notice;
    }
    // The service URL/detail is diagnostics: it renders only inside the
    // Diagnostics card, never in the product header.
    setText('daemon-url', detail || null);
  }

  /**
   * The product status line of the header: `Ready · <model>` while idle,
   * `Running · <n> agents · <state>` while a run is active. Service state is
   * invisible when healthy; only the two transient connection notices show.
   */
  function headerStatusText(snapshot) {
    var notice = snapshot && snapshot.connectionNotice;
    if (notice === 'connecting') {
      return 'Connecting to Faktor…';
    }
    if (notice === 'reconnecting') {
      return 'Reconnecting…';
    }
    var task = snapshot.task;
    var hasActiveRun = snapshot.busy === true || snapshot.activeRunId != null;
    var spec = runStateSpec(snapshot);
    if (hasActiveRun && task) {
      var agents = Array.isArray(snapshot.agents) ? snapshot.agents.length : 0;
      var label = spec !== null && spec.label && spec.label !== 'Ready' ? spec.label : 'Running';
      var parts = [label];
      if (agents > 0) {
        parts.push(agents + (agents === 1 ? ' agent' : ' agents'));
      }
      return parts.join(' · ');
    }
    var model = snapshot.session && snapshot.session.model ? String(snapshot.session.model) : '';
    if (spec !== null && spec.label && spec.label !== 'Ready') {
      return model.length > 0 ? spec.label + ' · ' + model : spec.label;
    }
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
        label.title = label.textContent;
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

  /**
   * A blocked stream is a PROBLEM, not a second card: it contributes one
   * source to the single inline attention surface. A transient reconnect is
   * already named by the header status line, so it adds no duplicate card.
   */
  function renderStreamRecovery(status) {
    setProblemSource('stream', problemFromStream(status));
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

  // ------------------------------------------------------ sandbox guidance I10
  // T32 is guidance only: the panel has NO write control, and the exact
  // command is presented for copying, never executed from here.

  function setSandboxGuidanceOpen(open) {
    var guidance = byId('sandbox-guidance');
    if (guidance) {
      guidance.hidden = open !== true;
    }
    var button = byId('btn-open-sandbox-guidance');
    if (button && typeof button.setAttribute === 'function') {
      button.setAttribute('aria-expanded', open === true ? 'true' : 'false');
    }
  }

  function copySandboxCommand() {
    var commandNode = byId('sandbox-command');
    var status = byId('sandbox-copy-status');
    var text =
      commandNode && typeof commandNode.textContent === 'string' ? commandNode.textContent : '';
    var copied = function () {
      if (status) {
        status.textContent = 'Copied: ' + text;
      }
    };
    var fallback = function () {
      if (commandNode && typeof commandNode.setAttribute === 'function') {
        commandNode.setAttribute('tabindex', '0');
      }
      if (commandNode && typeof commandNode.focus === 'function') {
        commandNode.focus();
      }
      if (status) {
        status.textContent =
          'Clipboard unavailable in this host; select and copy the command manually: ' + text;
      }
    };
    try {
      if (
        typeof navigator !== 'undefined' &&
        navigator.clipboard &&
        typeof navigator.clipboard.writeText === 'function'
      ) {
        navigator.clipboard.writeText(text).then(copied, fallback);
        return;
      }
    } catch (error) {
      // Fall through to the selection fallback.
    }
    fallback();
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
    // Content-key guard (same contract as the changes/history lists): an
    // unchanged board page re-posted by a stream delta must not re-create
    // the post rows or the composer draft affordances. The card visibility
    // is part of the key (the board lives in the Agents subview).
    var guardKey =
      activeSubview + '#' + hash128(jsonForHash(board === undefined ? null : board));
    if (renderedBoardKey !== null && renderedBoardKey === guardKey) {
      return;
    }
    renderedBoardKey = guardKey;
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
    // The board is the Coordination surface of the Inspect / Agents subview.
    var card = byId('board-card');
    if (card) {
      card.hidden = activeSubview !== 'agents';
    }
  }

  // ---------------------------------------------------- preflight/readiness I06

  /** The exact action dispatch for the preflight card + Diagnostics mirror. */
  function dispatchPreflightAction(key) {
    if (key === 'openProject') {
      vscode.postMessage({ type: 'openProject' });
    } else if (key === 'startService') {
      vscode.postMessage({ type: 'startDaemon' });
    } else if (key === 'locateDaemon') {
      vscode.postMessage({ type: 'locateDaemon' });
    } else if (key === 'retryStart') {
      vscode.postMessage({ type: 'retryStart' });
    } else if (key === 'reviewTrust') {
      vscode.postMessage({ type: 'reviewTrust' });
    } else if (key === 'manageWorkspaceTrust') {
      vscode.postMessage({ type: 'manageWorkspaceTrust' });
    } else if (key === 'openDiagnostics') {
      openView('diagnostics');
    } else if (key === 'startAnyway') {
      vscode.postMessage({ type: 'confirmStart' });
    } else if (key === 'cancelConfirm') {
      vscode.postMessage({ type: 'cancelConfirm' });
    } else if (key === 'cancelStart') {
      vscode.postMessage({ type: 'cancelStart' });
    } else if (key === 'openSetupGuidance') {
      setSandboxGuidanceOpen(true);
    }
  }

  function dispatchStreamRecoveryAction(key) {
    if (key === 'wait') {
      recoveryWaitDismissed = true;
      renderStreamRecovery(streamStatusValue);
    } else if (key === 'refresh') {
      vscode.postMessage({ type: 'refresh' });
    } else if (key === 'reconnectStream') {
      vscode.postMessage({ type: 'recoverStream' });
    } else if (key === 'refreshSnapshot') {
      vscode.postMessage({ type: 'refresh' });
    }
  }

  function preflightDotClass(state) {
    if (state === 'ready') {
      return 'dot-running';
    }
    if (state === 'starting' || state === 'unverified') {
      return 'dot-starting';
    }
    if (state === 'refused' || state === 'missing') {
      return 'dot-error';
    }
    return 'dot-stopped';
  }

  /** The Diagnostics Readiness row: the typed readiness outcome. */
  function readinessText(view) {
    if (!view || typeof view.readiness !== 'string') {
      return 'not requested';
    }
    if (view.readiness === 'idle') {
      return 'not requested';
    }
    if (view.readiness === 'waiting') {
      return 'waiting for GET /native/ready';
    }
    if (view.readiness === 'ready') {
      return 'ready';
    }
    return String(view.readinessDetail || view.readiness);
  }

  /** Honest index-coverage label; never flattened into "ready". */
  function indexCoverageText(snapshot) {
    var coverage = snapshot && snapshot.indexCoverage;
    if (!coverage) {
      return 'not reported';
    }
    var counters = '';
    if (coverage.coverage) {
      counters = coverage.coverage.files_indexed + '/' + coverage.coverage.files_seen + ' files';
    }
    if (coverage.freshness === 'partial') {
      return 'partial ' + counters;
    }
    if (coverage.freshness === 'stale_while_rebuilding') {
      return 'stale while rebuilding';
    }
    if (coverage.freshness === 'current') {
      return 'current';
    }
    return 'unknown';
  }

  function preflightDetailText(view) {
    if (!view) {
      return null;
    }
    if (typeof view.readinessDetail === 'string' && view.readinessDetail.length > 0 && view.readiness !== 'ready') {
      return view.readinessDetail;
    }
    if (typeof view.failureDetail === 'string' && view.failureDetail.length > 0) {
      return view.failureDetail;
    }
    if (view.state === 'unverified' || view.state === 'refused') {
      return typeof view.trustDetail === 'string' && view.trustDetail.length > 0 ? view.trustDetail : null;
    }
    return null;
  }

  function renderPreflightActions(containerId, view) {
    var container = byId(containerId);
    if (!container) {
      return;
    }
    var focus = captureFocus(container);
    clear(container);
    var actions = view && Array.isArray(view.actions) ? view.actions : [];
    for (var i = 0; i < actions.length; i++) {
      (function (action) {
        if (!action || typeof action.label !== 'string' || action.label.length === 0) {
          return;
        }
        var button = document.createElement('button');
        button.type = 'button';
        button.className = 'preflight-action';
        button.textContent = action.label;
        button.addEventListener('click', function () {
          dispatchPreflightAction(action.key);
        });
        container.appendChild(button);
      })(actions[i]);
    }
    restoreFocus(container, focus);
  }

  /**
   * The normal surface shows ONLY genuine authority, through the ONE inline
   * attention card. Healthy, connecting and reconnecting states use the
   * transient header line instead, so a normal user never sees daemon or
   * readiness vocabulary.
   */
  function renderServiceAuthority(authority) {
    setProblemSource('authority', problemFromAuthority(authority));
  }

  /** Diagnostics → Service: the exact supervised lifecycle detail. */
  function renderServiceDiagnostics(service) {
    if (service) {
      setText('machine-label', service.phase);
    }
    setText('svc-executable', service && service.executable ? service.executable : null);
    setText('svc-port', service && service.port != null ? String(service.port) : null);
    setText('svc-process', service && service.pid != null ? 'pid ' + service.pid : null);
    setText(
      'svc-restarts',
      service
        ? String(service.restarts) + ' / ' + String(service.restartBudget)
        : null,
    );
    setText('svc-last-failure', service && service.lastFailure ? service.lastFailure : null);
  }

  function renderPreflightMirror(view) {
    lastPreflight = view && typeof view.state === 'string' ? view : null;
    view = lastPreflight;
    var mirror = byId('diagnostics-preflight');
    if (!mirror) {
      return;
    }
    setText('readiness-label', readinessText(view));
    setText('trust-label', view && view.trustLabel ? view.trustLabel : null);
    if (view === null) {
      mirror.hidden = true;
      return;
    }
    mirror.hidden = false;
    var dot = byId('diag-preflight-dot');
    if (dot) {
      dot.className = 'dot ' + preflightDotClass(view.state);
    }
    setText('diag-preflight-label', view.label);
    setText('diag-preflight-copy', view.copy);
    var lines = [];
    if (typeof view.executable === 'string' && view.executable.length > 0) {
      lines.push('Executable: ' + view.executable);
    }
    if (typeof view.trustLabel === 'string' && view.trustLabel.length > 0) {
      lines.push(
        'Trust: ' +
          view.trustLabel +
          (typeof view.trustDetail === 'string' && view.trustDetail.length > 0
            ? ' — ' + view.trustDetail
            : ''),
      );
    }
    if (typeof view.readinessDetail === 'string' && view.readinessDetail.length > 0) {
      lines.push('Readiness: ' + view.readinessDetail);
    }
    if (typeof view.failureDetail === 'string' && view.failureDetail.length > 0) {
      lines.push('Last start failure: ' + view.failureDetail);
    }
    var detailNode = byId('diag-preflight-detail');
    if (detailNode) {
      detailNode.hidden = lines.length === 0;
      detailNode.textContent = lines.join('\n');
    }
    renderPreflightActions('diag-preflight-actions', view);
  }

  function usdMicro(value) {
    var raw = String(value === null || value === undefined ? '' : value);
    if (!/^[0-9]+$/.test(raw)) {
      return null;
    }
    var micro = BigInt(raw);
    if (micro === 0n) return '$0.00';
    if (micro < 10000n) return 'under $0.01';
    var whole = micro / 1000000n;
    var remainder = micro % 1000000n;
    var cents = (remainder + 5000n) / 10000n;
    if (cents >= 100n) {
      whole += 1n;
      cents -= 100n;
    }
    return '$' + whole.toString() + '.' + cents.toString().padStart(2, '0');
  }

  function dispatchUxAction(action) {
    if (!action || typeof action.key !== 'string') return;
    if (action.key === 'openProviderSettings') {
      vscode.postMessage({ type: 'openProviderSettings' });
      return;
    }
    if (action.key === 'reconnect' || action.key === 'resumeRun') {
      vscode.postMessage({ type: 'uxAction', action: action.key });
      return;
    }
    if (action.key === 'retryStart') {
      requestStart();
      return;
    }
    if (action.key === 'openChecks') {
      openView('inspect');
      selectSubviewTab('subtab-verification');
      return;
    }
    if (action.key === 'openDiagnostics') {
      openView('diagnostics');
      return;
    }
    if (action.key === 'reply') {
      var goal = byId('goal');
      if (goal && typeof goal.focus === 'function') goal.focus();
    }
  }

  /**
   * One attention problem. Sources never append cards; they replace their
   * own slot and the renderer selects exactly ONE problem by priority.
   * `key` carries the visible copy and technical tail so an identical error
   * can never stack, while a changed tail re-renders in place.
   */
  function attentionProblem(title, cause, action, actionKind, technical, source) {
    var titleText = String(title == null ? '' : title).trim();
    if (titleText.length === 0) {
      return null;
    }
    var causeText = String(cause == null ? '' : cause).trim();
    var techText = String(technical == null ? '' : technical).trim();
    return {
      source: source,
      title: titleText,
      cause: causeText,
      action: action && typeof action.key === 'string' ? action : null,
      actionKind: actionKind,
      technical: techText,
      key:
        source +
        ':' +
        hash128(titleText + '\u0000' + causeText + '\u0000' + techText),
    };
  }

  function problemFromAttention(snapshot) {
    var ux = snapshot && snapshot.ux ? snapshot.ux : null;
    var items = ux && Array.isArray(ux.attention) ? ux.attention : [];
    // Approval is the dedicated decision card below; never duplicate it in
    // the problem surface.
    var visible = [];
    for (var i = 0; i < items.length; i++) {
      if (items[i] && items[i].kind !== 'Approval') {
        visible.push(items[i]);
      }
    }
    if (visible.length === 0) {
      return null;
    }
    var first = visible[0];
    // Machine vocabulary (machine/run/proof/stream/daemon state) is
    // Diagnostics-only: the chat projection never copies it into the card.
    var technical = [];
    for (var r = 1; r < visible.length; r++) {
      technical.push(
        String(visible[r].label || '') +
          (visible[r].detail ? ' — ' + String(visible[r].detail) : ''),
      );
    }
    return attentionProblem(
      first.label || 'Needs attention',
      first.detail || '',
      first.action || null,
      'ux',
      technical.join('\n'),
      'attention',
    );
  }

  function problemFromAuthority(authority) {
    if (!authority) {
      return null;
    }
    var action =
      Array.isArray(authority.actions) && authority.actions.length > 0
        ? authority.actions[0]
        : null;
    return attentionProblem(
      authority.label,
      authority.copy,
      action,
      'authority',
      authority.detail,
      'authority',
    );
  }

  function problemFromStream(status) {
    var paused = status === 'protocol_blocked' || streamBlockedReason !== null;
    if (!paused) {
      return null;
    }
    return attentionProblem(
      STREAM_RECOVERY.paused.label,
      STREAM_RECOVERY.paused.copy,
      STREAM_RECOVERY.paused.actions[0],
      'stream',
      streamBlockedReason || 'the durable event stream is blocked',
      'stream',
    );
  }

  function problemFromUserError(error) {
    if (!error || typeof error !== 'object') {
      return null;
    }
    var technical =
      typeof error.technical === 'string' && error.technical.length > 0
        ? error.technical
        : 'no further detail was provided';
    var code = typeof error.code === 'string' && error.code.length > 0 ? error.code : null;
    return attentionProblem(
      typeof error.summary === 'string' && error.summary.length > 0
        ? error.summary
        : 'Faktor hit a problem',
      typeof error.hint === 'string' ? error.hint : '',
      error.action && typeof error.action.key === 'string' ? error.action : null,
      'userError',
      code !== null ? '[' + code + '] ' + technical : technical,
      'user',
    );
  }

  function dispatchProblem(problem) {
    if (!problem || !problem.action) {
      return;
    }
    if (problem.actionKind === 'authority') {
      dispatchPreflightAction(problem.action.key);
    } else if (problem.actionKind === 'userError') {
      dispatchUserErrorAction(problem.action.key);
    } else if (problem.actionKind === 'stream') {
      dispatchStreamRecoveryAction(problem.action.key);
    } else {
      dispatchUxAction(problem.action);
    }
  }

  function setProblemSource(source, problem) {
    var current = problemSources[source];
    var currentKey = current === null ? null : current.key;
    var nextKey = problem === null ? null : problem.key;
    if (currentKey === nextKey) {
      return;
    }
    problemSources[source] = problem;
    renderProblem();
  }

  /** The single inline attention card. Absent sources omit their section. */
  function renderProblem() {
    var card = byId('attention-card');
    if (!card) {
      return;
    }
    var order = ['authority', 'user', 'stream', 'attention'];
    var active = [];
    var chosen = null;
    for (var i = 0; i < order.length; i++) {
      var problem = problemSources[order[i]];
      if (problem !== null) {
        active.push(problem);
        if (chosen === null) {
          chosen = problem;
        }
      }
    }
    // Calm tasks and non-actionable conditions stay silent: the chat card
    // appears only when a genuinely actionable cause is served.
    if (chosen === null || chosen.action === null) {
      card.hidden = true;
      renderedProblemKey = null;
      setText('attention-card-title', 'Needs attention');
      var emptyCause = byId('attention-card-cause');
      if (emptyCause) {
        emptyCause.textContent = '';
        emptyCause.hidden = true;
      }
      clear(byId('attention-card-actions'));
      var emptyDetails = byId('attention-technical');
      if (emptyDetails) emptyDetails.hidden = true;
      clear(byId('attention-technical-body'));
      return;
    }
    var technical = [];
    for (var p = 0; p < active.length; p++) {
      var lines = String(active[p].technical || '').split('\n');
      for (var l = 0; l < lines.length; l++) {
        var value = lines[l].trim();
        if (value.length > 0 && technical.indexOf(value) === -1) {
          technical.push(value);
        }
      }
    }
    var key = chosen.key + '#' + hash128(technical.join('\n'));
    if (renderedProblemKey === key) {
      return;
    }
    renderedProblemKey = key;
    card.hidden = false;
    card.setAttribute('data-problem-kind', chosen.source);
    var icon = byId('attention-card-icon');
    if (icon) {
      icon.className = 'attention-icon attention-icon-' + chosen.source;
    }
    setText('attention-card-title', chosen.title);
    var causeNode = byId('attention-card-cause');
    if (causeNode) {
      causeNode.textContent = chosen.cause;
      causeNode.hidden = chosen.cause.length === 0;
    }
    var actions = byId('attention-card-actions');
    clear(actions);
    if (actions && chosen.action && typeof chosen.action.label === 'string') {
      var button = document.createElement('button');
      button.type = 'button';
      button.className = 'primary-action attention-action';
      button.textContent = chosen.action.label;
      (function (problem) {
        button.addEventListener('click', function () {
          dispatchProblem(problem);
        });
      })(chosen);
      actions.appendChild(button);
    }
    var details = byId('attention-technical');
    var body = byId('attention-technical-body');
    if (details && body) {
      body.textContent = technical.join('\n');
      details.hidden = technical.length === 0;
    }
  }

  function renderAttention(snapshot) {
    setProblemSource('attention', problemFromAttention(snapshot));
  }

  function renderOverviewSummary(snapshot) {
    var ux = snapshot && snapshot.ux ? snapshot.ux : null;
    var list = byId('overview-summary-list');
    if (!list) return;
    var status = byId('overview-status');
    if (status) {
      status.hidden = ux === null;
      if (ux !== null) {
        setText('overview-status-label', ux.presentationLabel);
        setText('overview-status-copy', ux.presentationDetail);
      }
    }
    clear(list);
    if (ux === null) return;
    var rows = [
      ['What it is doing', ux.summary.doing],
      ['Anything wrong', ux.summary.wrong],
      ['What changed', ux.summary.changed],
      ['Checks', ux.summary.checks],
      ['Cost', ux.summary.cost],
    ];
    for (var i = 0; i < rows.length; i++) {
      if (rows[i][1] === null || rows[i][1] === undefined || String(rows[i][1]).length === 0) {
        continue;
      }
      var dt = document.createElement('dt');
      dt.textContent = rows[i][0];
      var dd = document.createElement('dd');
      dd.textContent = String(rows[i][1]);
      list.appendChild(dt);
      list.appendChild(dd);
    }
  }

  function renderAccount(snapshot) {
    var panel = snapshot ? snapshot.usagePanel : null;
    var status = snapshot && typeof snapshot.accountStatus === 'string' ? snapshot.accountStatus : 'not_read';
    var copy = byId('account-copy');
    var lines = byId('account-lines');
    var advanced = byId('account-advanced');
    if (!copy || !lines) return;
    clear(lines);
    if (status === 'not_read' || (status === 'not_connected' && (panel === null || panel === undefined))) {
      copy.textContent = 'Account not connected. Billing and usage are optional and only load when you check.';
      if (advanced) advanced.textContent = 'No account credential is configured for this machine.';
      return;
    }
    if (status === 'unavailable' || panel === null || panel === undefined || panel.state !== 'ok') {
      copy.textContent = 'Account not connected.';
      if (advanced) {
        advanced.textContent = panel && panel.reason ? String(panel.reason) : 'The account read was refused.';
      }
      return;
    }
    copy.textContent = 'Account connected.';
    var rows = [];
    if (panel.planId) rows.push(['Plan', String(panel.planId)]);
    if (panel.subscription && panel.subscription.state) {
      rows.push(['Subscription', String(panel.subscription.state)]);
    }
    if (panel.period && typeof panel.period.totalTokens === 'number') {
      rows.push(['Tokens this period', formatCount(panel.period.totalTokens)]);
    }
    if (panel.period && panel.period.managedCostMicro) {
      var managed = usdMicro(panel.period.managedCostMicro);
      if (managed !== null) rows.push(['Managed spend', managed]);
    }
    if (panel.credits && panel.credits.balanceMicro) {
      var balance = usdMicro(panel.credits.balanceMicro);
      if (balance !== null) rows.push(['Credits', balance]);
    }
    for (var i = 0; i < rows.length; i++) {
      var dt = document.createElement('dt');
      dt.textContent = rows[i][0];
      var dd = document.createElement('dd');
      dd.textContent = rows[i][1];
      lines.appendChild(dt);
      lines.appendChild(dd);
    }
    if (advanced) {
      advanced.textContent = panel.reason ? String(panel.reason) : 'Served by the optional control-plane account.';
    }
  }

  function renderSnapshot(snapshot) {
    if (!snapshot) {
      return;
    }
    renderProbe('snapshot');
    var sessionId = snapshot.session ? String(snapshot.session.id) : null;
    if (currentSessionId !== null && sessionId !== currentSessionId) {
      // The host clears its store on a session switch (uploads are bound to
      // the session); the panel drops the same visible set and the pending
      // submission identity so a stale id can never leak into a new session.
      attachments = [];
      renderAttachments();
      // Transcript ownership is per conversation: drop the window, height
      // cache, content keys and pin/anchor state so the new conversation
      // starts at its own bottom instead of inheriting the old position.
      transcriptKey = null;
      transcriptEntryKeys = null;
      transcriptEntryStructures = null;
      transcriptEntries = null;
      transcriptRevision = null;
      transcriptWindowed = false;
      transcriptWindowStart = 0;
      transcriptWindowEnd = 0;
      transcriptHeights = [];
      transcriptPrefix = [0];
      transcriptPinned = true;
      transcriptAnchor = null;
      transcriptLastWrittenTop = 0;
    }
    currentSessionId = sessionId;
    machineStateValue =
      typeof snapshot.machineState === 'string' && snapshot.machineState.length > 0
        ? snapshot.machineState
        : 'unknown';
    runActive = snapshot.busy === true || snapshot.activeRunId != null;
    streamStatusValue =
      typeof snapshot.streamStatus === 'string' && snapshot.streamStatus.length > 0
        ? snapshot.streamStatus
        : 'stopped';
    if (streamStatusValue !== 'connecting' && streamStatusValue !== 'retrying') {
      // A settled status re-arms the T27 card after an explicit Wait.
      recoveryWaitDismissed = false;
    }
    setDaemon(snapshot.daemon, snapshot.daemonDetail, snapshot.connectionNotice);
    setText('daemon-text', headerStatusText(snapshot));
    setText('session-title', snapshot.session ? snapshot.session.title : 'none');
    setText('machine-label', snapshot.machineLabel || snapshot.machineState);
    setText('stream-status', streamStatusLabel(snapshot.streamStatus));
    setText('index-coverage', indexCoverageText(snapshot));
    renderDiagnosticsState(snapshot);
    renderComposerModel(snapshot.session);
    renderAttention(snapshot);
    renderOverviewSummary(snapshot);
    renderAccount(snapshot);
    renderServiceAuthority(snapshot.serviceAuthority || null);
    renderServiceDiagnostics(snapshot.service || null);
    renderPreflightMirror(snapshot.preflight);
    renderComposerState();
    renderStreamRecovery(streamStatusValue);
    // Initialize the ONE problem surface even when every source is absent
    // (the markup starts hidden; the fake DOM and an empty snapshot agree).
    renderProblem();
    renderTask(snapshot);
    renderPermissions(snapshot.permissions);
    renderInspectContent(snapshot);
    renderBreadcrumb(snapshot);
    renderHistory(snapshot);
    renderOutcomes(snapshot);
    renderWelcome(snapshot.transcript);
    renderTranscript(snapshot.transcript, snapshot.transcriptRevision);
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
      // A raw failure that the host no longer serves is gone; clear its user
      // problem so the ONE card never outlives the condition.
      if (lastRenderedError !== null) {
        setProblemSource('user', null);
      }
      lastRenderedError = null;
    } else {
      // The RAW last error is Diagnostics-only. A healthy/calm chat never
      // gets a generic "hit a problem" card from it; only a cause-specific,
      // actionable projection may surface in the chat body.
      lastRenderedError = lastError;
    }
  }

  /**
   * Diagnostics → Run: the ONLY surface that carries raw served machine
   * vocabulary. The chat body reads the human projection, never these rows.
   */
  function renderDiagnosticsState(snapshot) {
    var runs = snapshot && Array.isArray(snapshot.runs) ? snapshot.runs : [];
    var newest = runs.length > 0 ? runs[0] : null;
    setText('diag-machine-state', snapshot ? snapshot.machineState : null);
    setText('diag-run-state', newest && newest.state != null ? newest.state : null);
    setText('diag-proof-state', snapshot ? snapshot.proofTaskState : null);
    setText('diag-stream-state', snapshot ? snapshot.streamStatus : null);
    setText('diag-daemon-state', snapshot ? snapshot.daemon : null);
    setText('diag-last-error', snapshot ? snapshot.lastError : null);
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
    if (node.textContent !== text) {
      node.textContent = text;
    }
    if (text.length > 0) {
      var title = 'Session model: ' + text;
      if (!node.getAttribute || node.getAttribute('title') !== title) {
        node.setAttribute('title', title);
      }
    } else if (node.removeAttribute) {
      node.removeAttribute('title');
    }
  }

  /**
   * Transient informational notices are screen-reader-only; a visible error
   * is a problem and therefore uses the ONE attention card. No second
   * visible notice surface exists.
   */
  function showNotice(level, message) {
    var textValue = String(message);
    if (level === 'error') {
      showUserError({
        kind: 'unexpected',
        summary: 'Faktor hit a problem',
        hint: textValue,
        action: null,
        technical: textValue,
        code: null,
      });
      return;
    }
    var notices = byId('notices');
    var node = document.createElement('div');
    node.className = 'notice';
    node.textContent = textValue;
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
    if (key === 'startService' || key === 'retryStart') {
      vscode.postMessage({ type: 'startDaemon' });
    } else if (key === 'reconnectStream') {
      vscode.postMessage({ type: 'recoverStream' });
    } else if (key === 'refresh') {
      vscode.postMessage({ type: 'refresh' });
    } else if (key === 'attachFiles') {
      vscode.postMessage({ type: 'attachPick' });
    } else if (key === 'openDiagnostics') {
      openView('diagnostics');
    } else if (key === 'locateDaemon') {
      vscode.postMessage({ type: 'locateDaemon' });
    } else if (key === 'reviewTrust') {
      vscode.postMessage({ type: 'reviewTrust' });
    } else if (key === 'manageWorkspaceTrust') {
      vscode.postMessage({ type: 'manageWorkspaceTrust' });
    } else if (key === 'focusComposer') {
      var goalNode = byId('goal');
      if (goalNode && typeof goalNode.focus === 'function') {
        goalNode.focus();
      }
    }
  }

  /**
   * A structured user error feeds the SAME single attention card. Identical
   * errors (same visible copy and same technical tail) are ignored, so a
   * repeated host post can never stack a second card.
   */
  function showUserError(error) {
    var problem = problemFromUserError(error);
    if (problem === null) {
      return;
    }
    // One user slot: an identical error is a no-op, a different one replaces
    // it, and a recurrence after recovery is shown again.
    setProblemSource('user', problem);
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
      'btn-intent-ask',
      'btn-intent-task',
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
    renderComposerState();
  }

  // ------------------------------------------------------- message coalescing
  // The host posts one full snapshot per durable change. A fast stream can
  // deliver hundreds per second; rendering each one is wasted work because a
  // newer full snapshot supersedes the previous one. Consecutive snapshots
  // are therefore collapsed to the LATEST one and flushed on the next
  // animation frame (with a bounded timer fallback when frames are throttled,
  // e.g. a hidden panel). Non-snapshot messages flush the pending snapshot
  // first, so evidence/start/permission results keep their exact order.
  var pendingSnapshot = null;
  var snapshotFlushScheduled = false;
  var snapshotFlushHandle = null;
  var snapshotFlushTimer = null;

  function flushPendingSnapshot() {
    if (!snapshotFlushScheduled && pendingSnapshot === null) {
      return;
    }
    snapshotFlushScheduled = false;
    if (snapshotFlushTimer !== null) {
      clearTimeout(snapshotFlushTimer);
      snapshotFlushTimer = null;
    }
    if (snapshotFlushHandle !== null) {
      if (typeof window.cancelAnimationFrame === 'function') {
        window.cancelAnimationFrame(snapshotFlushHandle);
      }
      snapshotFlushHandle = null;
    }
    var next = pendingSnapshot;
    pendingSnapshot = null;
    if (next !== null) {
      renderSnapshot(next);
    }
  }

  function scheduleSnapshotFlush() {
    if (snapshotFlushScheduled) {
      renderProbe('snapshot-coalesced');
      return;
    }
    snapshotFlushScheduled = true;
    if (typeof window.requestAnimationFrame === 'function') {
      snapshotFlushHandle = window.requestAnimationFrame(flushPendingSnapshot);
    }
    if (snapshotFlushScheduled) {
      // Bounded: a throttled/hidden webview must still apply the newest state.
      snapshotFlushTimer = setTimeout(flushPendingSnapshot, 64);
    }
  }

  window.addEventListener('message', function (event) {
    var message = event.data || {};
    renderProbe('message');
    // A non-snapshot message must observe the state that preceded it: apply
    // the pending snapshot synchronously, then handle the message.
    if (message.type !== 'snapshot') {
      flushPendingSnapshot();
    }
    if (message.type === 'snapshot') {
      pendingSnapshot = message.snapshot;
      scheduleSnapshotFlush();
    } else if (message.type === 'faktorShowView') {
      // The host selected an internal destination of this SAME panel. No
      // panel, column or sidebar is created; the local view router moves.
      var requestedView = message.view === 'chat' ? 'work' : message.view;
      if (VIEW_PANELS[requestedView]) {
        openView(requestedView);
      }
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
    } else if (message.type === 'permissionResult') {
      // Answered by the host for EVERY reply outcome, correlated by the
      // request id: the per-request lock is released, a refusal keeps the
      // request visible with its exact reason, and the buttons re-enable.
      // Nothing is retried implicitly; a retry is always a new user click.
      var resultId = String(message.permissionId == null ? '' : message.permissionId);
      permissionInFlight.delete(resultId);
      if (message.ok !== true) {
        var refusalReason =
          typeof message.reason === 'string' && message.reason.length > 0
            ? message.reason.slice(0, MAX_PERMISSION_REASON_CHARS)
            : 'the reply was refused';
        rememberPermissionRefusal(resultId, refusalReason);
        renderPermissions(lastPermissions);
      } else {
        // The accepted request is about to disappear from the pending card;
        // a stale refusal line from an earlier attempt must not outlive it.
        forgetPermissionRefusal(resultId);
      }
    } else if (message.type === 'startResult') {
      // The explicit result releases the submitting lock on EVERY outcome.
      // A failed start kept the draft: T08 `Start response unavailable`
      // renders ONLY when the host classified the failure retryable (a lost
      // response reuses the SAME logical submission); a typed refusal keeps
      // the draft but ends that submission. The retry itself is always the
      // OPERATOR's next Run task.
      var ok = message.ok === true;
      startFailed = !ok && message.retryable === true;
      setSubmitting(false);
      var goalNode = byId('goal');
      if (goalNode) {
        goalNode.value = composerPolicy.afterStart(goalNode.value, message.goal, ok);
        autoGrowGoal();
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
        clearContractRowControls();
        clearRunBudgetControls();
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
        // A fresh pause re-arms the card after an explicit Wait.
        recoveryWaitDismissed = false;
      }
      renderStreamCursor();
      renderStreamRecovery(streamStatusValue);
    } else if (message.type === 'streamRecovery') {
      streamRecovering = message.recovering === true;
      if (streamRecovering) {
        recoveryWaitDismissed = false;
      }
      renderStreamRecovery(streamStatusValue);
    } else if (message.type === 'userError') {
      showUserError(message.error);
    } else if (message.type === 'changeReview') {
      // The host resolved the selected change's immutable basis: `opened`
      // names the served evidence id now in the native editor; `unavailable`
      // asks for the exact in-view state (nothing live was read).
      changeReviewState = {
        path: typeof message.path === 'string' ? message.path : '',
        state: message.state === 'opened' ? 'opened' : 'unavailable',
        evidenceId: typeof message.evidenceId === 'number' ? message.evidenceId : null,
        reason: typeof message.reason === 'string' ? message.reason : null,
      };
      renderChangeReviewState();
    } else if (message.type === 'notice') {
      showNotice(message.level, message.message);
      // A refused Open (or a failed history refresh) answers with a notice:
      // release the row lock so the operator can retry without waiting for
      // an unrelated snapshot.
      if (historyOpenInFlight !== null) {
        historyOpenInFlight = null;
        renderHistory(lastSnapshot);
      }
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
   * The "Acceptance & contract" rows: one entry per non-blank line, bounded
   * at the host boundary (the host refuses a malformed/oversized envelope
   * with a typed error rather than silently dropping an entry). All-blank
   * fields produce no member at all (today's default path).
   */
  function contractRowsFromControls() {
    var ids = ['contract-constraints', 'contract-invariants', 'contract-risks', 'contract-obligations'];
    var members = ['constraints', 'invariants', 'risks', 'proofObligations'];
    var rows = {};
    var any = false;
    for (var i = 0; i < ids.length; i++) {
      var node = byId(ids[i]);
      var raw = node && typeof node.value === 'string' ? node.value.split('\n') : [];
      var entries = [];
      for (var j = 0; j < raw.length; j++) {
        var value = raw[j].trim();
        if (value.length > 0) {
          entries.push(value);
        }
      }
      rows[members[i]] = entries;
      if (entries.length > 0) {
        any = true;
      }
    }
    return any ? rows : null;
  }

  /** A successful start clears the per-run contract rows (kept on failure). */
  function clearContractRowControls() {
    var ids = ['contract-constraints', 'contract-invariants', 'contract-risks', 'contract-obligations'];
    for (var i = 0; i < ids.length; i++) {
      var node = byId(ids[i]);
      if (node) {
        node.value = '';
      }
    }
  }

  /**
   * The two per-run budget fields of the "Acceptance & contract" disclosure
   * (I02). Empty = the setting is used; a non-empty value rides THIS
   * submission verbatim (the host validates and refuses typed on garbage).
   */
  function runBudgetFromControls() {
    var tokensNode = byId('contract-budget-tokens');
    var costNode = byId('contract-budget-cost');
    var tokens = tokensNode && typeof tokensNode.value === 'string' ? tokensNode.value.trim() : '';
    var cost = costNode && typeof costNode.value === 'string' ? costNode.value.trim() : '';
    var budget = {};
    if (tokens.length > 0) {
      budget.tokens = tokens;
    }
    if (cost.length > 0) {
      // Human money in the composer, exact micro on the wire. `$5.00` and
      // `5.00` mean five dollars; an explicit `5000000µ$` (or a bare
      // integer) stays exact micro for compatibility.
      var dollars = /^\$?([0-9]+)(?:\.([0-9]{1,6}))?$/.exec(cost);
      if (dollars && (cost.charAt(0) === '$' || cost.indexOf('.') !== -1)) {
        var whole = BigInt(dollars[1]);
        var fraction = (dollars[2] || '').padEnd(6, '0');
        budget.cost = (whole * 1000000n + BigInt(fraction || '0')).toString();
      } else {
        budget.cost = cost;
      }
    }
    return budget;
  }

  /** A successful start clears the per-run budget fields (kept on failure). */
  function clearRunBudgetControls() {
    var ids = ['contract-budget-tokens', 'contract-budget-cost'];
    for (var i = 0; i < ids.length; i++) {
      var node = byId(ids[i]);
      if (node) {
        node.value = '';
      }
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
    // Ask is a plain conversational turn: no contract, rows, budget or
    // completion member is ever attached. Task carries only what the user
    // authored in the ONE Task options disclosure.
    var message = { type: 'sendGoal', goal: goal, intent: composerIntent };
    if (attachments.length > 0) {
      var ids = [];
      for (var i = 0; i < attachments.length; i++) {
        ids.push(attachments[i].id);
      }
      message.attachmentIds = ids;
    }
    if (composerIntent === 'task') {
      var contract = completionContractFromControls();
      var contractRows = contractRowsFromControls();
      var budget = runBudgetFromControls();
      if (contract) {
        message.completionContract = contract;
      }
      if (contractRows) {
        message.contractRows = contractRows;
      }
      if (budget.tokens !== undefined) {
        message.budgetTokens = budget.tokens;
      }
      if (budget.cost !== undefined) {
        message.budgetCostMicro = budget.cost;
      }
    }
    transcriptPinRequested = true;
    transcriptPinned = true;
    transcriptAnchor = null;
    // A new submission supersedes a kept failed start (T08 -> T07).
    startFailed = false;
    setSubmitting(true);
    // Run task becomes disabled while the submission is pending; keep
    // keyboard focus in the composer instead of dropping it to the body.
    var goalNode = byId('goal');
    if (goalNode && typeof goalNode.focus === 'function') {
      goalNode.focus();
    }
    vscode.postMessage(message);
  }

  function setComposerIntent(intent) {
    composerIntent = intent === 'task' ? 'task' : 'ask';
    var form = byId('composer');
    if (form && typeof form.setAttribute === 'function') {
      form.setAttribute('data-intent', composerIntent);
    }
    var ask = byId('btn-intent-ask');
    var task = byId('btn-intent-task');
    if (ask) ask.setAttribute('aria-checked', composerIntent === 'ask' ? 'true' : 'false');
    if (task) task.setAttribute('aria-checked', composerIntent === 'task' ? 'true' : 'false');
  }

  byId('btn-intent-ask').addEventListener('click', function () {
    setComposerIntent('ask');
  });
  byId('btn-intent-task').addEventListener('click', function () {
    setComposerIntent('task');
  });
  setComposerIntent('ask');
  autoGrowGoal();

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

  byId('goal').addEventListener('input', autoGrowGoal);

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

  // Escape closes the overflow menu first (returning focus to its EXACT
  // trigger — the `⋯` button) and otherwise dismisses the transient notice.
  // Escape never cancels a run.
  window.addEventListener('keydown', function (event) {
    if (!event || event.key !== 'Escape') {
      return;
    }
    if (overflowOpen) {
      setOverflowOpen(false);
      var trigger = byId('btn-overflow');
      if (trigger && typeof trigger.focus === 'function') {
        trigger.focus();
      }
      return;
    }
    if (activeView !== 'work') {
      openView('work');
      return;
    }
    dismissTransient();
  });

  byId('btn-account-read').addEventListener('click', function () {
    vscode.postMessage({ type: 'readAccount' });
  });
  byId('btn-account-settings').addEventListener('click', function () {
    vscode.postMessage({ type: 'openAccountSettings' });
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
  // The `⋯` menu holds the destinations (Chat / Inspect / History /
  // Diagnostics) and the scoped Stop action. It is NOT a second header:
  // selecting an item swaps the full-window view and closes the menu.
  byId('btn-control-center').addEventListener('click', function () {
    if (activeView === 'settings') {
      openView('work');
      return;
    }
    openView('settings');
    vscode.postMessage({ type: 'openSettingsView' });
  });
  var backChat = byId('btn-back-chat');
  if (backChat) {
    backChat.addEventListener('click', function () {
      openView('work');
    });
  }
  var settingsBack = byId('settings-back-chat');
  if (settingsBack) {
    settingsBack.addEventListener('click', function () {
      openView('work');
    });
  }
  byId('btn-overflow').addEventListener('click', function () {
    toggleOverflow();
  });
  // One-shot diagnostic: report the exact rendered DOM to the host so a
  // stale/wrong surface can be identified from evidence instead of guesses.
  safeSetTimeout(function () {
    try {
      vscode.postMessage({
        type: 'uiDomDump',
        html: new XMLSerializer().serializeToString(document.documentElement).slice(0, 300000),
        mode: document.body.getAttribute('data-mode'),
        version: (byId('ui-version') || {}).textContent || '',
      });
    } catch (e) {}
  }, 1500);
  byId('menu-chat').addEventListener('click', function () {
    openView('work');
  });
  byId('menu-run-action').addEventListener('click', function () {
    var action = activeMenuAction;
    setOverflowOpen(false);
    if (action !== null) {
      dispatchUxAction(action);
    }
  });
  byId('menu-inspect').addEventListener('click', function () {
    openView('inspect');
  });
  byId('menu-history').addEventListener('click', function () {
    openView('history');
  });
  byId('menu-settings').addEventListener('click', function () {
    openView('settings');
    vscode.postMessage({ type: 'openSettingsView' });
  });
  byId('menu-diagnostics').addEventListener('click', function () {
    openView('diagnostics');
  });
  wireMenuNavigation();
  byId('menu-stop').addEventListener('click', function () {
    setOverflowOpen(false);
    vscode.postMessage({ type: 'cancelRun' });
  });
  byId('btn-open-sandbox-guidance').addEventListener('click', function () {
    setSandboxGuidanceOpen(true);
  });
  byId('btn-copy-sandbox-command').addEventListener('click', copySandboxCommand);
  // T32: the exact guidance command ships in the static markup; mirror it into
  // an empty code node so the copy action copies the visible text verbatim.
  var sandboxCommand = byId('sandbox-command');
  if (sandboxCommand && !sandboxCommand.textContent) {
    sandboxCommand.textContent = 'faktor sandbox choose';
  }
  // The guidance disclosure starts closed in every rendering context.
  setSandboxGuidanceOpen(false);
  byId('btn-refresh').addEventListener('click', function () {
    vscode.postMessage({ type: 'refresh' });
  });
  byId('btn-start').addEventListener('click', function () {
    vscode.postMessage({ type: 'startDaemon' });
  });
  byId('btn-stop').addEventListener('click', function () {
    vscode.postMessage({ type: 'stopDaemon' });
  });

  // ------------------------------------------------------- shell wiring
  // Chat is the default full-window surface. The Inspect subview tablist and
  // the History Conversations/Outcomes pair keep roving tabindex semantics
  // inside their own full-window views; the chat body has no tabs.
  wireTablist(
    ['subtab-overview', 'subtab-plan', 'subtab-changes', 'subtab-verification', 'subtab-agents'],
    selectSubviewTab,
  );
  wireTablist(
    ['history-tab-conversations', 'history-tab-outcomes', 'history-tab-account'],
    function (tabId, focusTab) {
      var key =
        tabId === 'history-tab-outcomes'
          ? 'outcomes'
          : tabId === 'history-tab-account'
            ? 'account'
            : 'conversations';
      setHistoryTab(key, focusTab);
    },
  );
  setActiveView('work');
  setActiveSubview('overview');
  setHistoryTab('conversations', false);

  // History: the bounded title search re-filters only the served rows; both
  // refresh affordances post ONE refreshHistory (never a full implicit
  // daemon start).
  byId('history-search').addEventListener('input', function () {
    var node = byId('history-search');
    historyQuery =
      node && typeof node.value === 'string'
        ? node.value.slice(0, historyPolicy.MAX_HISTORY_QUERY)
        : '';
    renderHistory(lastSnapshot);
  });
  byId('btn-history-refresh').addEventListener('click', function () {
    vscode.postMessage({ type: 'refreshHistory' });
  });
  byId('btn-history-refresh-unavailable').addEventListener('click', function () {
    vscode.postMessage({ type: 'refreshHistory' });
  });

  // I07: the Outcomes read is EXPLICIT (Refresh / Older runs); the top read
  // replaces the list, the cursor read appends through the composite cursor.
  byId('btn-outcomes-refresh').addEventListener('click', function () {
    outcomesReading = true;
    updateOutcomesControls(lastSnapshot ? lastSnapshot.outcomes : null);
    vscode.postMessage({ type: 'readOutcomes' });
  });
  byId('btn-outcomes-older').addEventListener('click', function () {
    var view = lastSnapshot ? lastSnapshot.outcomes : null;
    if (!view || view.state !== 'ok' || !view.nextCursor) {
      return;
    }
    outcomesReading = true;
    updateOutcomesControls(view);
    vscode.postMessage({
      type: 'readOutcomes',
      beforeMs: view.nextCursor.completedAtMs,
      beforeRun: view.nextCursor.runId,
    });
  });

  // I03: the scope toolbar of the selected change. Previous/Next are bounded
  // (disabled at the ends); Review asks the host to resolve and open the
  // immutable basis; Open claim selects the served criterion in Verification;
  // Return to run focuses the chat command and switches to Work; Recheck
  // re-runs the served reads and re-derives the current basis.
  byId('btn-change-prev').addEventListener('click', function () {
    selectChange(selectedIndexFor(rowsFromSnapshot()) - 1, rowsFromSnapshot());
  });
  byId('btn-change-next').addEventListener('click', function () {
    selectChange(selectedIndexFor(rowsFromSnapshot()) + 1, rowsFromSnapshot());
  });
  byId('btn-change-review').addEventListener('click', function () {
    var rows = rowsFromSnapshot();
    var index = selectedIndexFor(rows);
    if (index < 0) {
      return;
    }
    changeReviewState = null;
    renderChangeReviewState();
    vscode.postMessage({ type: 'reviewChange', path: String(rows[index].path) });
  });
  byId('btn-change-claim').addEventListener('click', openClaimForSelected);
  byId('btn-change-return').addEventListener('click', function () {
    setActiveView('work');
    vscode.postMessage({ type: 'openWork' });
  });
  function recheckChangesFromGesture() {
    vscode.postMessage({ type: 'recheckChanges' });
  }
  byId('btn-change-recheck').addEventListener('click', recheckChangesFromGesture);
  byId('btn-changes-recheck-unavailable').addEventListener('click', recheckChangesFromGesture);

  vscode.postMessage({ type: 'ready' });
})();
