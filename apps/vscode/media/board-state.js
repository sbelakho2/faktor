// Coordination-board presentation policy, shared by the built-in webview
// and the selftest.
//
// The durable board is a run-family surface (native GET/POST
// `/native/session/{id}/board`); the panel must render the header state
// truthfully (never fabricate posts or unread counts), bound every displayed
// line, and refuse an over-bound draft LOCALLY with the same byte limits the
// host re-validates (state.ts MAX_BOARD_*). These constants are parity-pinned
// by the selftest against the host exports.
//
// Loaded as a classic script in the webview (defines `FaktorBoard`) and
// importable from Node for tests via `module.exports`.
(function (root) {
  'use strict';

  // Keep in lockstep with state.ts (asserted by the selftest).
  var MAX_BOARD_SUBJECT_BYTES = 512;
  var MAX_BOARD_BODY_BYTES = 16 * 1024;
  // Mirrors JetBrains BoardPanel.MAX_BOARD_LINE_CHARS: one displayed line is
  // bounded, the full text stays behind the host's own bounds.
  var MAX_BOARD_LINE_CHARS = 240;
  // At most this many posts are rendered from one page (host pages cap at
  // MAX_BOARD_PAGE = 100); the oldest are dropped from the VIEW, never from
  // the durable board.
  var MAX_BOARD_RENDERED_POSTS = 50;

  function utf8Bytes(text) {
    if (typeof TextEncoder !== 'undefined') {
      return new TextEncoder().encode(text).length;
    }
    // Node < 11 fallback (the selftest runs on modern Node; this branch is
    // only for a hostile embedding without TextEncoder).
    return Buffer.byteLength(text, 'utf8');
  }

  // Invisible C0/C1 controls and explicit bidi embedding/override controls
  // are stripped from displayed lines (they are layout/spoofing vectors,
  // never human text; RTL letters and shaping are untouched).
  var BOARD_BIDI_CONTROLS = /[\u200e\u200f\u202a-\u202e\u2066-\u2069]/g;
  var BOARD_CONTROL_CHARS = /[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g;

  function stripBoardControls(value) {
    return value.replace(BOARD_BIDI_CONTROLS, '').replace(BOARD_CONTROL_CHARS, '');
  }

  /** Slice at `max` UTF-16 units, never splitting a surrogate pair. */
  function safeBoardSlice(value, max) {
    if (max <= 0) {
      return '';
    }
    if (value.length <= max) {
      return value;
    }
    var preceding = value.charCodeAt(max - 1);
    var next = value.charCodeAt(max);
    var splitsPair =
      preceding >= 0xd800 && preceding <= 0xdbff && next >= 0xdc00 && next <= 0xdfff;
    return value.slice(0, splitsPair ? max - 1 : max);
  }

  /** Bound one displayed line: never a raw unbounded daemon string. */
  function boundBoardLine(text) {
    var value = stripBoardControls(
      typeof text === 'string' ? text : String(text === null || text === undefined ? '' : text),
    );
    return value.length > MAX_BOARD_LINE_CHARS
      ? safeBoardSlice(value, MAX_BOARD_LINE_CHARS) + '\u2026'
      : value;
  }

  /**
   * The board header vocabulary. `null` = no read yet (never "0 posts");
   * unavailable names the typed reason; available carries the exact
   * revision/unread/post counts the host projected.
   */
  function boardHeader(board) {
    if (board === null || board === undefined) {
      return 'board: not read yet';
    }
    if (!board.available) {
      return 'board: unavailable (' + boundBoardLine(board.reason || 'reason unavailable') + ')';
    }
    var revision = board.revision === null || board.revision === undefined ? '?' : board.revision;
    var unread = board.unread === null || board.unread === undefined ? '?' : board.unread;
    var count = Array.isArray(board.posts) ? board.posts.length : 0;
    return 'board: rev=' + revision + ' unread=' + unread + ' posts=' + count;
  }

  /**
   * The local draft refusal (same bounds the host re-validates). Returns
   * null when the draft may be posted; otherwise a typed reason. The server
   * stays the durable guard.
   */
  function boardDraftRefusal(subject, body) {
    var subjectText = typeof subject === 'string' ? subject.trim() : '';
    var bodyText = typeof body === 'string' ? body : '';
    if (subjectText.length === 0) {
      return 'subject is required';
    }
    if (utf8Bytes(subjectText) > MAX_BOARD_SUBJECT_BYTES) {
      return 'subject exceeds ' + MAX_BOARD_SUBJECT_BYTES + ' bytes';
    }
    if (bodyText.trim().length === 0) {
      return 'body is required';
    }
    if (utf8Bytes(bodyText) > MAX_BOARD_BODY_BYTES) {
      return 'body exceeds ' + MAX_BOARD_BODY_BYTES + ' bytes';
    }
    return null;
  }

  /**
   * The bounded, presentation-only view of one board page: every string is
   * rendered as text and bounded, and the list length is capped. Nothing is
   * fabricated when the board is unavailable.
   */
  function boardPostsForDisplay(board, max) {
    if (board === null || board === undefined || !board.available || !Array.isArray(board.posts)) {
      return [];
    }
    var limit = typeof max === 'number' && max > 0 ? max : MAX_BOARD_RENDERED_POSTS;
    return board.posts.slice(0, limit).map(function (post) {
      return {
        id: String(post && post.id !== undefined ? post.id : ''),
        revision: post && post.revision !== undefined ? post.revision : null,
        author: boundBoardLine(post && post.author ? post.author : 'root'),
        subject: boundBoardLine(post && post.subject ? post.subject : ''),
        body: boundBoardLine(post && post.body ? post.body : ''),
      };
    });
  }

  var api = {
    MAX_BOARD_SUBJECT_BYTES: MAX_BOARD_SUBJECT_BYTES,
    MAX_BOARD_BODY_BYTES: MAX_BOARD_BODY_BYTES,
    MAX_BOARD_LINE_CHARS: MAX_BOARD_LINE_CHARS,
    MAX_BOARD_RENDERED_POSTS: MAX_BOARD_RENDERED_POSTS,
    boundBoardLine: boundBoardLine,
    boardHeader: boardHeader,
    boardDraftRefusal: boardDraftRefusal,
    boardPostsForDisplay: boardPostsForDisplay,
  };
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
  if (root) {
    root.FaktorBoard = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : null);
