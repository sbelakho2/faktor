// Pure, bounded markdown structure for the assistant transcript renderer.
//
// Contract (the renderer's safety rules, enforced HERE at parse time):
//  - NO HTML is ever parsed or produced: `<...>` is ordinary text; a hostile
//    message can only ever contribute plain characters to a Swing document.
//  - Links are parsed ONLY for the safe schemes http/https/mailto (see
//    [safeUrl]); every other scheme (`javascript:`, `file:`, `ide:`,
//    `command:`, `data:` …) degrades to literal label text with the raw URL
//    visible — nothing is ever "clickable" outside the allowlist, and no
//    IDE command can be invoked through a link.
//  - Everything is BOUNDED: block count, spans per block, inline nesting
//    depth, table columns/rows, fence language length. Excess collapses
//    into one plain paragraph of the caller's already-bounded text.
//  - Pure and headless: no Swing, no toolkit, no I/O. Incremental rendering
//    hangs off [settledEnd], the offset of the last completed block
//    boundary, so a growing live message re-renders only its tail.
package dev.faktor.frontend

/** One inline run with explicit style; never HTML, never a command. */
internal sealed class InlineSpan {
    data class Text(val text: String) : InlineSpan()

    data class Strong(val spans: List<InlineSpan>) : InlineSpan()

    data class Emphasis(val spans: List<InlineSpan>) : InlineSpan()

    data class CodeSpan(val text: String) : InlineSpan()

    /** A SAFE-scheme link (http/https/mailto) validated by [RichMarkdown.safeUrl]. */
    data class Link(val label: String, val url: String) : InlineSpan()

    fun plainText(): String = when (this) {
        is Text -> text
        is Strong -> spans.joinToString("") { it.plainText() }
        is Emphasis -> spans.joinToString("") { it.plainText() }
        is CodeSpan -> text
        is Link -> label
    }
}

internal enum class TableAlign { LEFT, RIGHT, CENTER }

/** One list item (indent level in 4-space units, capped). */
internal data class ListItem(val indent: Int, val spans: List<InlineSpan>)

/** One structural block of assistant markdown. */
internal sealed class MarkdownBlock {
    data class Heading(val level: Int, val spans: List<InlineSpan>) : MarkdownBlock()

    data class Paragraph(val spans: List<InlineSpan>) : MarkdownBlock()

    data class BulletList(val items: List<ListItem>) : MarkdownBlock()

    data class NumberedList(val startAt: Int, val items: List<ListItem>) : MarkdownBlock()

    data class Quote(val lines: List<List<InlineSpan>>) : MarkdownBlock()

    data class Code(val language: String, val code: String) : MarkdownBlock()

    data object Rule : MarkdownBlock()

    data class Table(
        val header: List<String>,
        val alignments: List<TableAlign>,
        val rows: List<List<String>>
    ) : MarkdownBlock()

    fun plainText(): String = when (this) {
        is Heading -> spans.joinToString("") { it.plainText() }
        is Paragraph -> spans.joinToString("") { it.plainText() }
        is BulletList -> items.joinToString("\n") { item ->
            "  ".repeat(item.indent) + "- " + item.spans.joinToString("") { it.plainText() }
        }
        is NumberedList -> items.mapIndexed { index, item ->
            "  ".repeat(item.indent) + "${startAt + index}. " +
                item.spans.joinToString("") { it.plainText() }
        }.joinToString("\n")
        is Quote -> lines.joinToString("\n") { line -> line.joinToString("") { it.plainText() } }
        is Code -> code
        is Rule -> "---"
        is Table -> (listOf(header) + rows).joinToString("\n") { row -> row.joinToString(" | ") }
    }
}

/**
 * The markdown scanner. Every entry point is pure and bounded; hostile
 * input can only ever produce a (possibly simplified) block list.
 */
internal object RichMarkdown {

    /** Hard cap of blocks per message (excess collapses to one paragraph). */
    const val MAX_BLOCKS = 2048

    /** Hard cap of inline spans per block. */
    const val MAX_SPANS = 512

    /** Inline emphasis nesting bound (strong/em inside strong/em). */
    const val MAX_INLINE_DEPTH = 3

    /** Table column / row caps (a pipe bomb can never grow a widget). */
    const val MAX_TABLE_COLUMNS = 16
    const val MAX_TABLE_ROWS = 128

    /** Fence language character class + length cap (sanitized, never a path). */
    private val FENCE_LANGUAGE = Regex("^[A-Za-z0-9_+.-]{1,32}$")
    private val FENCE_OPEN = Regex("^\\s*```\\s*([A-Za-z0-9_+.-]{0,32})\\s*$")
    private val FENCE_CLOSE = Regex("^\\s*```\\s*$")
    private val HEADING = Regex("^(#{1,6})\\s+(.*)$")
    private val RULE = Regex("^\\s{0,3}([-*_])(?:\\s*\\1){2,}\\s*$")
    private val BULLET = Regex("^(\\s*)([-*+])\\s+(.*)$")
    private val NUMBERED = Regex("^(\\s*)(\\d{1,9})[.)]\\s+(.*)$")
    private val QUOTE = Regex("^\\s*>\\s?(.*)$")
    private val TABLE_SEPARATOR =
        Regex("^\\s*\\|?\\s*:?-{1,}:?\\s*(\\|\\s*:?-{1,}:?\\s*)+\\|?\\s*$")

    /** Parse [text] into bounded blocks (never throws, never grows unbounded). */
    fun parse(text: String): List<MarkdownBlock> {
        if (text.isEmpty()) return emptyList()
        val lines = text.split('\n')
        val blocks = ArrayList<MarkdownBlock>()
        var index = 0
        while (index < lines.size) {
            if (blocks.size >= MAX_BLOCKS) {
                // Bounded: the remainder collapses into ONE plain paragraph.
                val rest = lines.subList(index, lines.size).joinToString("\n")
                blocks.add(MarkdownBlock.Paragraph(listOf(InlineSpan.Text(rest))))
                break
            }
            val line = lines[index]

            if (line.isBlank()) {
                index++
                continue
            }

            val fenceOpen = FENCE_OPEN.matchEntire(line)
            if (fenceOpen != null) {
                val language = fenceOpen.groupValues[1]
                    .takeIf { FENCE_LANGUAGE.matches(it) } ?: ""
                val code = StringBuilder()
                var closeIndex = index + 1
                while (closeIndex < lines.size && !FENCE_CLOSE.matches(lines[closeIndex])) {
                    if (code.isNotEmpty()) code.append('\n')
                    code.append(lines[closeIndex])
                    closeIndex++
                }
                blocks.add(MarkdownBlock.Code(language, code.toString()))
                index = closeIndex + 1
                continue
            }

            val heading = HEADING.matchEntire(line)
            if (heading != null) {
                blocks.add(
                    MarkdownBlock.Heading(
                        heading.groupValues[1].length.coerceIn(1, 6),
                        inline(heading.groupValues[2], 1)
                    )
                )
                index++
                continue
            }

            if (RULE.matches(line)) {
                blocks.add(MarkdownBlock.Rule)
                index++
                continue
            }

            // GFM pipe table: header row + separator row + body rows.
            if (line.contains('|') && index + 1 < lines.size &&
                TABLE_SEPARATOR.matches(lines[index + 1])
            ) {
                val header = splitRow(line)
                if (header.isNotEmpty()) {
                    val alignments = parseAlignments(lines[index + 1], header.size)
                    val rows = ArrayList<List<String>>()
                    var body = index + 2
                    while (body < lines.size && lines[body].contains('|') &&
                        lines[body].isNotBlank() && rows.size < MAX_TABLE_ROWS
                    ) {
                        val cells = splitRow(lines[body])
                        val padded = ArrayList<String>(header.size)
                        for (column in 0 until header.size) {
                            padded.add(cells.getOrElse(column) { "" })
                        }
                        rows.add(padded)
                        body++
                    }
                    blocks.add(
                        MarkdownBlock.Table(
                            header.take(MAX_TABLE_COLUMNS),
                            alignments,
                            rows.map { row -> row.take(MAX_TABLE_COLUMNS) }
                        )
                    )
                    index = body
                    continue
                }
            }

            val bullet = BULLET.matchEntire(line)
            val numbered = NUMBERED.matchEntire(line)
            if (bullet != null || numbered != null) {
                val isBullet = bullet != null
                val items = ArrayList<ListItem>()
                var startAt = 1
                var listIndex = index
                while (listIndex < lines.size) {
                    val current = lines[listIndex]
                    val match = if (isBullet) {
                        BULLET.matchEntire(current)
                    } else {
                        NUMBERED.matchEntire(current)
                    } ?: break
                    val indentChars = match.groupValues[1].length
                    val content = if (isBullet) match.groupValues[3] else match.groupValues[4]
                    if (!isBullet && items.isEmpty()) {
                        startAt = match.groupValues[2].toIntOrNull() ?: 1
                    }
                    if (items.size >= MAX_SPANS) break
                    items.add(ListItem((indentChars / 2).coerceIn(0, 6), inline(content, 1)))
                    listIndex++
                }
                if (items.isNotEmpty()) {
                    blocks.add(
                        if (isBullet) MarkdownBlock.BulletList(items) else MarkdownBlock.NumberedList(startAt, items)
                    )
                    index = listIndex
                    continue
                }
            }

            val quote = QUOTE.matchEntire(line)
            if (quote != null) {
                val quoteLines = ArrayList<List<InlineSpan>>()
                var quoteIndex = index
                while (quoteIndex < lines.size) {
                    val match = QUOTE.matchEntire(lines[quoteIndex]) ?: break
                    if (quoteLines.size >= MAX_SPANS) break
                    quoteLines.add(inline(match.groupValues[1], 1))
                    quoteIndex++
                }
                blocks.add(MarkdownBlock.Quote(quoteLines))
                index = quoteIndex
                continue
            }

            // Paragraph: consecutive ordinary lines until the next block start.
            val paragraphLines = ArrayList<String>()
            var paraIndex = index
            while (paraIndex < lines.size) {
                val current = lines[paraIndex]
                if (current.isBlank()) break
                if (FENCE_OPEN.matches(current) || HEADING.matches(current) ||
                    RULE.matches(current) || QUOTE.matches(current) ||
                    BULLET.matches(current) || NUMBERED.matches(current)
                ) break
                if (paragraphLines.isNotEmpty() &&
                    current.contains('|') && paraIndex + 1 < lines.size &&
                    TABLE_SEPARATOR.matches(lines[paraIndex + 1])
                ) break
                paragraphLines.add(current)
                paraIndex++
                if (paragraphLines.size > MAX_SPANS) break
            }
            blocks.add(MarkdownBlock.Paragraph(inline(paragraphLines.joinToString("\n"), 1)))
            index = paraIndex
        }
        return blocks
    }

    /**
     * The exclusive char offset of the last COMPLETED block boundary in
     * [text] — everything before it is settled (its block components keep
     * their Swing identity forever); everything after it is the streaming
     * tail that the renderer re-renders on each update.
     *
     * A boundary is: a heading line, a thematic rule, a closing fence, or a
     * blank line that follows content. (An unclosed fence never settles, so
     * a live code block stays the tail until its closing fence arrives.)
     */
    fun settledEnd(text: String): Int {
        var offset = 0
        var lastBoundary = 0
        var sawContent = false
        var inFence = false
        var index = 0
        while (index < text.length) {
            val newline = text.indexOf('\n', index)
            val lineEnd = if (newline < 0) text.length else newline
            val line = text.substring(index, lineEnd)
            if (inFence) {
                if (FENCE_CLOSE.matches(line)) {
                    inFence = false
                    lastBoundary = lineEnd + 1
                }
            } else if (FENCE_OPEN.matches(line)) {
                inFence = true
                sawContent = true
            } else if (HEADING.matches(line) || RULE.matches(line)) {
                lastBoundary = lineEnd + 1
                sawContent = true
            } else if (line.isBlank()) {
                if (sawContent) lastBoundary = lineEnd + 1
            } else {
                sawContent = true
            }
            offset = lineEnd + 1
            index = offset
        }
        return lastBoundary.coerceAtMost(text.length)
    }

    /**
     * The safe-scheme validator: absolute http/https/mailto URLs only.
     * Returns the trimmed URL, or null for every other scheme, control
     * character, or malformed URI. This is the ONLY gate that makes a link
     * interactive; everything else renders as literal text.
     */
    fun safeUrl(raw: String): String? {
        val url = raw.trim()
        if (url.isEmpty() || url.length > 2048) return null
        for (element in url) {
            if (element.code < 0x20 || element == '\u007f') return null
        }
        val lower = url.lowercase()
        if (!lower.startsWith("https://") && !lower.startsWith("http://") &&
            !lower.startsWith("mailto:")
        ) {
            return null
        }
        return try {
            val uri = java.net.URI(url)
            val scheme = uri.scheme?.lowercase() ?: return null
            when (scheme) {
                "http", "https" -> if (uri.host.isNullOrEmpty()) null else url
                "mailto" -> {
                    // An opaque mailto URI carries the address in the
                    // scheme-specific part (getPath() is null there).
                    val path = uri.path ?: uri.schemeSpecificPart ?: ""
                    if (path.contains('@') && path.length in 3..320) url else null
                }
                else -> null
            }
        } catch (e: Exception) {
            null
        }
    }

    // ------------------------------------------------------------- inline

    /**
     * Single-pass inline scanner with a hard nesting depth. Unmatched
     * delimiters stay literal; an unsafe link degrades to its literal
     * source text (label + URL both visible, nothing clickable).
     */
    private fun inline(source: String, depth: Int): List<InlineSpan> {
        if (source.isEmpty()) return emptyList()
        val spans = ArrayList<InlineSpan>()
        val buffer = StringBuilder()

        fun flush() {
            if (buffer.isNotEmpty() && spans.size < MAX_SPANS) {
                spans.add(InlineSpan.Text(buffer.toString()))
            }
            buffer.setLength(0)
        }

        var index = 0
        while (index < source.length && spans.size < MAX_SPANS) {
            val char = source[index]

            // Inline code: `code` (single-tick runs; content verbatim and
            // non-empty — an empty run stays literal streaming noise).
            if (char == '`') {
                val close = source.indexOf('`', index + 1)
                if (close > index + 1) {
                    flush()
                    spans.add(InlineSpan.CodeSpan(source.substring(index + 1, close)))
                    index = close + 1
                    continue
                }
            }

            // Safe link: [label](url).
            if (char == '[') {
                val closeLabel = indexOfUnescaped(source, ']', index + 1)
                if (closeLabel > index && closeLabel + 1 < source.length &&
                    source[closeLabel + 1] == '('
                ) {
                    val closeUrl = indexOfUnescaped(source, ')', closeLabel + 2)
                    if (closeUrl > closeLabel) {
                        val label = source.substring(index + 1, closeLabel)
                        val url = source.substring(closeLabel + 2, closeUrl)
                        val safe = safeUrl(url)
                        if (safe != null) {
                            flush()
                            spans.add(InlineSpan.Link(label.take(512), safe))
                            index = closeUrl + 1
                            continue
                        }
                        // Unsafe scheme: fall through as literal characters
                        // (the raw URL stays visible, nothing is clickable).
                    }
                }
            }

            // Strong / emphasis, only at a bounded depth.
            if (depth < MAX_INLINE_DEPTH &&
                (char == '*' || char == '_')
            ) {
                val marker = char
                val doubled = index + 1 < source.length && source[index + 1] == marker
                val tokenLength = if (doubled) 2 else 1
                val close = findClose(source, index + tokenLength, marker, doubled)
                if (close > index) {
                    val inner = source.substring(index + tokenLength, close)
                    if (inner.isNotEmpty() && !inner.isBlank()) {
                        flush()
                        val innerSpans = inline(inner, depth + 1)
                        spans.add(
                            if (doubled) InlineSpan.Strong(innerSpans)
                            else InlineSpan.Emphasis(innerSpans)
                        )
                        index = close + tokenLength
                        continue
                    }
                }
            }

            buffer.append(char)
            index++
        }
        if (index < source.length && spans.size < MAX_SPANS) {
            // Bounded overflow: the tail collapses into one literal span.
            buffer.append(source.substring(index))
        }
        flush()
        return spans.ifEmpty { listOf(InlineSpan.Text(source)) }
    }

    private fun indexOfUnescaped(text: String, target: Char, from: Int): Int {
        var index = from
        while (index < text.length) {
            if (text[index] == target) {
                var backslashes = 0
                var probe = index - 1
                while (probe >= 0 && text[probe] == '\\') {
                    backslashes++
                    probe--
                }
                if (backslashes % 2 == 0) return index
            }
            index++
        }
        return -1
    }

    /** The closing delimiter run for emphasis (paired, non-empty inner). */
    private fun findClose(text: String, from: Int, marker: Char, doubled: Boolean): Int {
        var index = from
        while (index < text.length) {
            if (text[index] == marker) {
                var run = 0
                var probe = index
                while (probe < text.length && text[probe] == marker) {
                    run++
                    probe++
                }
                val need = if (doubled) 2 else 1
                if (run >= need) return index
                index = probe
                continue
            }
            index++
        }
        return -1
    }

    private fun splitRow(line: String): List<String> {
        val trimmed = line.trim().trim('|')
        return trimmed.split('|').map { cell -> cell.trim().take(128) }
    }

    private fun parseAlignments(separator: String, columns: Int): List<TableAlign> {
        val cells = splitRow(separator)
        return (0 until columns).map { column ->
            val cell = cells.getOrNull(column) ?: ""
            val left = cell.startsWith(':')
            val right = cell.endsWith(':')
            when {
                left && right -> TableAlign.CENTER
                right -> TableAlign.RIGHT
                else -> TableAlign.LEFT
            }
        }
    }
}
