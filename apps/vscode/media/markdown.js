// Safe incremental Markdown for assistant transcript text (audit item 9).
// Hand-written, zero-dependency, CSP-safe: EVERY node is created with
// createElement and every string lands through textContent — raw HTML and
// SVG are inert BY CONSTRUCTION (never innerHTML, never a parsed tag), and
// links are HOST-VALIDATED (only http/https/mailto/vscode-webview schemes;
// javascript:, data:, file:, vbscript:, command: and friends render as
// plain text, never as anchors).
//
// Supported blocks: ATX headings, paragraphs, unordered/ordered lists
// (flat), fenced code with a language label + Copy button, pipe tables,
// and horizontal rules. Inline: `code`, **strong**, *emphasis*/*, and
// [label](url) with scheme validation.
//
// Incremental contract: `FaktorMarkdown.render(container, text)` keeps the
// DOM nodes of every block that was terminated BEFORE the previous partial
// tail (an append-only streaming update re-renders only the tail), so
// selection/focus inside completed blocks survives a growing reply. An
// unterminated fence at the tail renders as an open code block marked
// `data-partial` (partial-tail tolerant parse).
//
// Loaded as a classic script in the webview (defines `FaktorMarkdown`) and
// importable from Node for tests via `module.exports`.
(function (root) {
  'use strict';

  var MAX_INLINE_NESTING = 4;

  var SAFE_SCHEMES = ['http:', 'https:', 'mailto:', 'vscode-webview:'];

  /** TRUE when `url` may become a real anchor (host-validated scheme). */
  function safeUrl(url) {
    var text = String(url == null ? '' : url).trim();
    if (text.length === 0 || text.length > 2048) {
      return null;
    }
    // Absolute URLs: scheme must be on the allowlist. Protocol-relative and
    // bare paths are rejected too — a transcript link is never a navigation
    // primitive inside the panel.
    var scheme = /^([A-Za-z][A-Za-z0-9+.-]*):/.exec(text);
    if (scheme === null) {
      return null;
    }
    var lower = scheme[1].toLowerCase() + ':';
    if (SAFE_SCHEMES.indexOf(lower) === -1) {
      return null;
    }
    return text;
  }

  /** Split into lines without ever producing a trailing phantom line. */
  function linesOf(text) {
    return String(text == null ? '' : text).split('\n');
  }

  function fenceInfo(line) {
    var match = /^ {0,3}```(.*)$/.exec(line);
    if (match !== null) {
      return { marker: '```', info: match[1].trim() };
    }
    match = /^ {0,3}~~~(.*)$/.exec(line);
    if (match !== null) {
      return { marker: '~~~', info: match[1].trim() };
    }
    return null;
  }

  function headingOf(line) {
    var match = /^(#{1,6})\s+(.*)$/.exec(line);
    return match === null ? null : { level: match[1].length, text: match[2].replace(/\s+#+\s*$/, '') };
  }

  function listItemOf(line) {
    var match = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/.exec(line);
    if (match === null) {
      return null;
    }
    return {
      indent: match[1].length,
      ordered: /\d/.test(match[2].charAt(0)),
      text: match[3],
    };
  }

  function tableRowOf(line) {
    var trimmed = String(line).trim();
    if (trimmed.charAt(0) !== '|' || trimmed.length < 2) {
      return null;
    }
    var body = trimmed.slice(1, trimmed.lastIndexOf('|') === trimmed.length - 1 ? -1 : undefined);
    var cells = [];
    var current = '';
    var escaped = false;
    for (var i = 0; i < body.length; i += 1) {
      var ch = body.charAt(i);
      if (escaped) {
        current += ch;
        escaped = false;
        continue;
      }
      if (ch === '\\') {
        escaped = true;
        continue;
      }
      if (ch === '|') {
        cells.push(current.trim());
        current = '';
        continue;
      }
      current += ch;
    }
    cells.push(current.trim());
    return cells;
  }

  function isTableSeparator(line) {
    var cells = tableRowOf(line);
    if (cells === null || cells.length === 0) {
      return false;
    }
    for (var i = 0; i < cells.length; i += 1) {
      if (!/^:?-{3,}:?$/.test(cells[i])) {
        return false;
      }
    }
    return true;
  }

  /**
   * Parse `text` into blocks with byte offsets, marking the FIRST
   * unterminated block (the partial tail) — the incremental renderer keeps
   * every block before it.
   */
  function parse(text) {
    var source = String(text == null ? '' : text);
    var lines = linesOf(source);
    var blocks = [];
    var offset = 0;
    var partialStart = source.length;
    var i = 0;
    while (i < lines.length) {
      var line = lines[i];
      var start = offset;
      var blank = line.trim().length === 0;
      if (blank) {
        offset += line.length + 1;
        i += 1;
        continue;
      }
      var fence = fenceInfo(line);
      if (fence !== null) {
        var codeLines = [];
        var closed = false;
        offset += line.length + 1;
        i += 1;
        while (i < lines.length) {
          var close = fenceInfo(lines[i]);
          if (close !== null && close.marker === fence.marker && close.info === '') {
            closed = true;
            offset += lines[i].length + 1;
            i += 1;
            break;
          }
          codeLines.push(lines[i]);
          offset += lines[i].length + 1;
          i += 1;
        }
        blocks.push({
          kind: 'code',
          lang: fence.info.split(/\s+/)[0] || '',
          code: codeLines.join('\n'),
          closed: closed,
          start: start,
          end: offset,
        });
        if (!closed) {
          partialStart = start;
          break;
        }
        continue;
      }
      var heading = headingOf(line);
      if (heading !== null) {
        offset += line.length + 1;
        i += 1;
        blocks.push({ kind: 'heading', level: heading.level, text: heading.text, start: start, end: offset });
        continue;
      }
      if (/^ {0,3}([-*_])( *\1){2,} *$/.test(line)) {
        offset += line.length + 1;
        i += 1;
        blocks.push({ kind: 'hr', start: start, end: offset });
        continue;
      }
      var row = tableRowOf(line);
      if (row !== null && i + 1 < lines.length && isTableSeparator(lines[i + 1])) {
        var header = row;
        var rows = [];
        offset += line.length + 1;
        i += 1;
        offset += lines[i].length + 1;
        i += 1;
        while (i < lines.length) {
          var nextRow = tableRowOf(lines[i]);
          if (nextRow === null) {
            break;
          }
          rows.push(nextRow);
          offset += lines[i].length + 1;
          i += 1;
        }
        blocks.push({ kind: 'table', header: header, rows: rows, start: start, end: offset });
        continue;
      }
      var firstItem = listItemOf(line);
      if (firstItem !== null) {
        var ordered = firstItem.ordered;
        var items = [];
        while (i < lines.length) {
          var item = listItemOf(lines[i]);
          if (item === null || item.ordered !== ordered) {
            break;
          }
          items.push(item.text);
          offset += lines[i].length + 1;
          i += 1;
          // Lazy continuation of the previous item.
          while (
            i < lines.length &&
            lines[i].trim().length > 0 &&
            listItemOf(lines[i]) === null &&
            fenceInfo(lines[i]) === null &&
            headingOf(lines[i]) === null
          ) {
            items[items.length - 1] += ' ' + lines[i].trim();
            offset += lines[i].length + 1;
            i += 1;
          }
        }
        blocks.push({ kind: 'list', ordered: ordered, items: items, start: start, end: offset });
        continue;
      }
      var paraLines = [line];
      offset += line.length + 1;
      i += 1;
      while (
        i < lines.length &&
        lines[i].trim().length > 0 &&
        fenceInfo(lines[i]) === null &&
        headingOf(lines[i]) === null &&
        listItemOf(lines[i]) === null &&
        tableRowOf(lines[i]) === null &&
        !/^ {0,3}([-*_])( *\1){2,} *$/.test(lines[i])
      ) {
        paraLines.push(lines[i]);
        offset += lines[i].length + 1;
        i += 1;
      }
      var terminated = i < lines.length && lines[i].trim().length === 0;
      blocks.push({ kind: 'paragraph', text: paraLines.join('\n'), start: start, end: offset });
      if (!terminated) {
        partialStart = start;
        break;
      }
    }
    return { blocks: blocks, partialStart: partialStart, source: source };
  }

  // ------------------------------------------------------------ inline

  function appendText(doc, container, text) {
    if (text.length > 0) {
      container.appendChild(doc.createTextNode(text));
    }
  }

  /**
   * Render inline syntax into `container`. Depth-bounded scanner: code
   * spans win, then links, then strong, then emphasis; unmatched markers
   * render literally. Raw HTML is text.
   */
  function renderInline(container, text, depth, doc) {
    if (depth > MAX_INLINE_NESTING) {
      appendText(container, String(text));
      return;
    }
    var source = String(text == null ? '' : text);
    var i = 0;
    var plain = '';
    while (i < source.length) {
      var ch = source.charAt(i);
      if (ch === '\\' && i + 1 < source.length && '\\`*_[]()'.indexOf(source.charAt(i + 1)) !== -1) {
        plain += source.charAt(i + 1);
        i += 2;
        continue;
      }
      if (ch === '`') {
        var close = source.indexOf('`', i + 1);
        if (close !== -1) {
          appendText(doc, container, plain);
          plain = '';
          var code = doc.createElement('code');
          code.textContent = source.slice(i + 1, close);
          container.appendChild(code);
          i = close + 1;
          continue;
        }
      }
      if (ch === '[') {
        var labelEnd = source.indexOf('](', i);
        var urlEnd = labelEnd === -1 ? -1 : source.indexOf(')', labelEnd + 2);
        if (labelEnd !== -1 && urlEnd !== -1) {
          var label = source.slice(i + 1, labelEnd);
          var url = safeUrl(source.slice(labelEnd + 2, urlEnd));
          appendText(doc, container, plain);
          plain = '';
          if (url !== null) {
            var anchor = doc.createElement('a');
            anchor.setAttribute('href', url);
            anchor.setAttribute('rel', 'noreferrer');
            renderInline(anchor, label, depth + 1, doc);
            container.appendChild(anchor);
          } else {
            // Rejected scheme: the label and the literal URL stay text —
            // never an anchor, never a navigation primitive.
            renderInline(container, label, depth + 1, doc);
            appendText(doc, container, ' (' + source.slice(labelEnd + 2, urlEnd) + ')');
          }
          i = urlEnd + 1;
          continue;
        }
      }
      if ((ch === '*' || ch === '_') && i + 1 < source.length) {
        var marker = ch;
        var doubled = source.charAt(i + 1) === marker;
        var token = doubled ? marker + marker : marker;
        var end = source.indexOf(token, i + token.length);
        if (end !== -1 && end > i + token.length) {
          var inner = source.slice(i + token.length, end);
          if (inner.trim().length > 0 && inner.indexOf('\n') === -1) {
            appendText(doc, container, plain);
            plain = '';
            var strong = doubled ? doc.createElement('strong') : doc.createElement('em');
            renderInline(strong, inner, depth + 1, doc);
            container.appendChild(strong);
            i = end + token.length;
            continue;
          }
        }
      }
      plain += ch;
      i += 1;
    }
    appendText(doc, container, plain);
  }

  // ------------------------------------------------------------ blocks

  function renderBlock(doc, block) {
    if (block.kind === 'heading') {
      var h = doc.createElement('h' + String(Math.min(6, Math.max(1, block.level))));
      renderInline(h, block.text, 0, doc);
      return h;
    }
    if (block.kind === 'hr') {
      return doc.createElement('hr');
    }
    if (block.kind === 'code') {
      var figure = doc.createElement('div');
      figure.className = 'md-code';
      var bar = doc.createElement('div');
      bar.className = 'md-code-bar';
      var label = doc.createElement('span');
      label.className = 'md-code-lang';
      label.textContent = block.lang.length > 0 ? block.lang : 'code';
      bar.appendChild(label);
      var copy = doc.createElement('button');
      copy.type = 'button';
      copy.className = 'md-code-copy';
      copy.textContent = 'Copy';
      var codeText = block.code;
      copy.addEventListener('click', function () {
        var clipboard = typeof navigator !== 'undefined' ? navigator.clipboard : null;
        if (clipboard && typeof clipboard.writeText === 'function') {
          clipboard.writeText(codeText).then(
            function () {
              copy.textContent = 'Copied';
            },
            function () {
              copy.textContent = 'Copy failed';
            },
          );
        } else {
          copy.textContent = 'Copy unavailable';
        }
      });
      bar.appendChild(copy);
      figure.appendChild(bar);
      var pre = doc.createElement('pre');
      var code = doc.createElement('code');
      code.textContent = codeText;
      pre.appendChild(code);
      figure.appendChild(pre);
      if (!block.closed) {
        figure.setAttribute('data-partial', 'true');
      }
      return figure;
    }
    if (block.kind === 'list') {
      var list = doc.createElement(block.ordered ? 'ol' : 'ul');
      for (var i = 0; i < block.items.length; i += 1) {
        var li = doc.createElement('li');
        renderInline(li, block.items[i], 0, doc);
        list.appendChild(li);
      }
      return list;
    }
    if (block.kind === 'table') {
      var table = doc.createElement('table');
      var thead = doc.createElement('thead');
      var headRow = doc.createElement('tr');
      for (var hcell = 0; hcell < block.header.length; hcell += 1) {
        var th = doc.createElement('th');
        renderInline(th, block.header[hcell], 0, doc);
        headRow.appendChild(th);
      }
      thead.appendChild(headRow);
      table.appendChild(thead);
      var tbody = doc.createElement('tbody');
      for (var r = 0; r < block.rows.length; r += 1) {
        var tr = doc.createElement('tr');
        for (var c = 0; c < block.rows[r].length; c += 1) {
          var td = doc.createElement('td');
          renderInline(td, block.rows[r][c], 0, doc);
          tr.appendChild(td);
        }
        tbody.appendChild(tr);
      }
      table.appendChild(tbody);
      return table;
    }
    var p = doc.createElement('p');
    renderInline(p, block.text, 0, doc);
    return p;
  }

  /**
   * Incremental render into `container`. Completed block nodes keep their
   * identity across append-only updates; only the partial tail is
   * re-parsed and re-rendered. A full replace (different earlier text)
   * re-renders every block.
   */
  function render(container, text) {
    if (!container) {
      return;
    }
    var doc = container.ownerDocument || (typeof document !== 'undefined' ? document : null);
    if (!doc) {
      return;
    }
    var source = String(text == null ? '' : text);
    var state = container.__faktorMarkdown || null;
    if (
      state !== null &&
      typeof state.source === 'string' &&
      source.length >= state.source.length &&
      source.lastIndexOf(state.source, 0) === 0
    ) {
      // Append-only growth: keep every block that was terminated before the
      // previous partial tail.
      var parsed = parse(source);
      var keep = 0;
      while (keep < state.blocks.length && keep < parsed.blocks.length) {
        var before = state.blocks[keep];
        var after = parsed.blocks[keep];
        if (before.kind !== after.kind || before.end !== after.end || before.start !== after.start) {
          break;
        }
        keep += 1;
      }
      // A block that was PARTIAL before may have closed now — it must be
      // re-rendered, never kept (its node was the tolerant open form).
      if (keep > 0 && keep <= state.blocks.length && state.partialStart < state.source.length) {
        var lastKept = state.blocks[keep - 1];
        if (lastKept.end > state.partialStart) {
          keep -= 1;
        }
      }
      while (container.childNodes.length > keep) {
        container.removeChild(container.lastChild);
      }
      for (var b = keep; b < parsed.blocks.length; b += 1) {
        container.appendChild(renderBlock(doc, parsed.blocks[b]));
      }
      container.__faktorMarkdown = {
        source: source,
        blocks: parsed.blocks,
        partialStart: parsed.partialStart,
      };
      return;
    }
    var full = parse(source);
    while (container.firstChild) {
      container.removeChild(container.firstChild);
    }
    for (var i = 0; i < full.blocks.length; i += 1) {
      container.appendChild(renderBlock(doc, full.blocks[i]));
    }
    container.__faktorMarkdown = {
      source: source,
      blocks: full.blocks,
      partialStart: full.partialStart,
    };
  }

  var api = {
    parse: parse,
    render: render,
    safeUrl: safeUrl,
  };
  if (typeof module !== 'undefined' && module.exports) {
    module.exports = api;
  }
  if (root) {
    root.FaktorMarkdown = api;
  }
})(typeof globalThis !== 'undefined' ? globalThis : null);
