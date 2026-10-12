// History surface policy, shared by the built-in webview and the selftest.
//
// The durable conversation list is served by `GET /native/sessions`
// (newest-first; see crates/server/src/native/session.rs): title, provider,
// model, state and the ADDITIVE created_ms. The policy below renders only
// those served fields: a missing date is an explicit absence (never a
// fabricated one), and the listing is named as capped because the route
// caps at MAX_NATIVE_SESSION_LISTING (1000) — the surface must never claim
// to be the complete durable history.
//
// Loaded as a classic script in the webview (defines `FaktorHistory`) and
// importable from Node for tests via `module.exports`.
(function (root) {
  'use strict';

  /** The T31 unavailable state, verbatim. */
  var HISTORY_UNAVAILABLE_LABEL = 'History unavailable';
  var HISTORY_UNAVAILABLE_COPY = 'The conversation list could not be read.';
  var HISTORY_UNAVAILABLE_ACTION = 'Refresh history';

  /** The server's hard listing cap (MAX_NATIVE_SESSION_LISTING). */
  var HISTORY_CAP = 1000;
  var HISTORY_CAP_NOTE =
    'Newest first; the service serves at most ' + HISTORY_CAP + ' conversations.';
  /**
   * The honest separation: a conversation's state is not run verification
   * evidence. A history row must never read as a verification claim.
   */
  var HISTORY_SEPARATION_NOTE =
    'Conversation state is separate from run verification evidence; open a conversation and use Inspect / Verification for evidence.';
  var HISTORY_EMPTY = 'No conversations are served yet.';
  var HISTORY_NO_MATCH = 'No conversation title matches this search.';

  /** Bounds: rendered rows and the normalized search query. */
  var MAX_HISTORY_ROWS = 200;
  var MAX_HISTORY_QUERY = 200;
  var MAX_HISTORY_TITLE = 200;
  var MAX_HISTORY_STATE = 64;

  /**
   * ISO text of one served epoch-ms creation time, or null when the value is
   * absent/unrenderable: an honest absence, never a fabricated date.
   */
  function sessionDateLabel(createdMs) {
    if (typeof createdMs !== 'number' || !isFinite(createdMs)) {
      return null;
    }
    var date = new Date(createdMs);
    if (!isFinite(date.getTime())) {
      return null;
    }
    return date.toISOString();
  }

  /** The display title of one served row (never blank, never an id). */
  function historyTitle(entry) {
    if (entry && typeof entry.title === 'string' && entry.title.trim().length > 0) {
      var trimmed = entry.title.trim();
      return trimmed.length > MAX_HISTORY_TITLE ? trimmed.slice(0, MAX_HISTORY_TITLE) + '...' : trimmed;
    }
    return 'Untitled conversation';
  }

  /** The served state tag of one row, byte-bounded; absent reads `unknown`. */
  function historyStateLabel(entry) {
    if (entry && typeof entry.state === 'string' && entry.state.trim().length > 0) {
      var state = entry.state.trim();
      return state.length > MAX_HISTORY_STATE ? state.slice(0, MAX_HISTORY_STATE) : state;
    }
    return 'unknown';
  }

  /**
   * The bounded client-side title search: case-insensitive substring match
   * over the served titles. Returns the rendered rows (at most
   * MAX_HISTORY_ROWS), the total number of matches and whether the render is
   * capped — so a capped render can say so instead of implying the list ends.
   */
  function historyRows(sessions, query) {
    var all = Array.isArray(sessions) ? sessions : [];
    var normalized =
      typeof query === 'string' ? query.trim().toLowerCase().slice(0, MAX_HISTORY_QUERY) : '';
    var matches = [];
    for (var i = 0; i < all.length; i++) {
      var entry = all[i];
      if (!entry || typeof entry !== 'object') {
        continue;
      }
      if (normalized.length > 0 && historyTitle(entry).toLowerCase().indexOf(normalized) === -1) {
        continue;
      }
      matches.push(entry);
    }
    var rows = matches.slice(0, MAX_HISTORY_ROWS);
    return {
      rows: rows,
      total: matches.length,
      shown: rows.length,
      capped: matches.length > rows.length,
    };
  }

  /** The bounded status line (counts + the cap note). */
  function historyStatusText(result, query) {
    var r = result || { total: 0, shown: 0, capped: false };
    var normalized = typeof query === 'string' ? query.trim().slice(0, MAX_HISTORY_QUERY) : '';
    if (r.total === 0) {
      return normalized.length > 0 ? HISTORY_NO_MATCH : HISTORY_EMPTY;
    }
    var line;
    if (normalized.length > 0) {
      line = r.shown + ' of ' + r.total + ' match "' + normalized + '"';
    } else {
      line = r.total + (r.total === 1 ? ' conversation' : ' conversations');
    }
    if (r.capped) {
      line += ' \u00b7 showing the first ' + r.shown;
    }
    return line + ' \u00b7 ' + HISTORY_CAP_NOTE;
  }

  var api = {
    HISTORY_UNAVAILABLE_LABEL: HISTORY_UNAVAILABLE_LABEL,
    HISTORY_UNAVAILABLE_COPY: HISTORY_UNAVAILABLE_COPY,
    HISTORY_UNAVAILABLE_ACTION: HISTORY_UNAVAILABLE_ACTION,
    HISTORY_CAP: HISTORY_CAP,
    HISTORY_CAP_NOTE: HISTORY_CAP_NOTE,
    HISTORY_SEPARATION_NOTE: HISTORY_SEPARATION_NOTE,
    HISTORY_EMPTY: HISTORY_EMPTY,
    HISTORY_NO_MATCH: HISTORY_NO_MATCH,
    MAX_HISTORY_ROWS: MAX_HISTORY_ROWS,
    MAX_HISTORY_QUERY: MAX_HISTORY_QUERY,
    sessionDateLabel: sessionDateLabel,
    historyTitle: historyTitle,
    historyStateLabel: historyStateLabel,
    historyRows: historyRows,
    historyStatusText: historyStatusText,
  };
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
  if (root) {
    root.FaktorHistory = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : null);
