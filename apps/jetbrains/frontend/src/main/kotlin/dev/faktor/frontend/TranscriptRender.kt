// The component-based transcript renderer: stable You/Faktor identities,
// real styled Markdown blocks, safe links and copyable code — all headless-
// safe (no toolkit call outside [HostClipboard]/[HostLinks], which guard
// every AWT touch and expose test seams instead).
//
// Structure:
//  - [TranscriptView] hosts one keyed [TranscriptCell] per transcript row;
//    completed cells keep their Swing component identity forever (selection
//    and focus survive later arrivals);
//  - an assistant cell's body is an [AssistantBody]: SETTLED markdown blocks
//    keep their components; only the streaming TAIL is re-rendered per
//    update ([RichMarkdown.settledEnd] is the settle boundary);
//  - code blocks render with a language label and a Copy action riding the
//    host clipboard through [HostClipboard] (headless: recorded, never a
//    toolkit call);
//  - links render underlined with the URL in the tooltip; a click re-
//    validates the scheme through [HostLinks] (http/https/mailto only) —
//    no IDE command, no file:, no javascript:, ever;
//  - tables render plain-but-aligned in monospace; structure stays visible;
//  - raw HTML is never parsed or produced: hostile text can only ever become
//    characters in a Swing document.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Desktop
import java.awt.Dimension
import java.awt.FlowLayout
import java.awt.Font
import java.awt.GraphicsEnvironment
import java.awt.Toolkit
import java.awt.datatransfer.StringSelection
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import java.net.URI
import java.util.ArrayList
import java.util.LinkedHashMap
import javax.swing.BorderFactory
import javax.swing.Box
import javax.swing.BoxLayout
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JSeparator
import javax.swing.JTextArea
import javax.swing.JTextPane
import javax.swing.Timer
import javax.swing.border.EmptyBorder
import javax.swing.text.SimpleAttributeSet
import javax.swing.text.StyleConstants
import javax.swing.text.StyledDocument

/** One rendered transcript row: identity, turn group and kind-specific text. */
internal data class TranscriptRowPayload(
    val kind: String,
    val text: String,
    val seq: Long,
    val rowId: String,
    val group: String,
    val label: String? = null
) {
    fun identity(): String? = when (kind) {
        KIND_USER -> "You"
        KIND_ASSISTANT -> "Faktor"
        else -> null
    }

    /** The plain-text projection (the smoke/diagnostics surface). */
    fun plainText(showIdentity: Boolean): String = when (kind) {
        KIND_USER -> "You: $text"
        KIND_ASSISTANT -> if (showIdentity) "Faktor: $text" else text
        KIND_TOOL -> "[" + (label ?: "tool") + "] " + text
        KIND_APPROVAL -> "[approval] $text"
        else -> text
    }

    /**
     * The tooltip/diagnostic identity: seq and ids live HERE (and in
     * Diagnostics), never in the visible line.
     */
    fun diagnostic(): String {
        val who = identity() ?: kind.replaceFirstChar { it.uppercase() }
        val where = if (seq >= 0) "#$seq" else "draft"
        val groupSuffix = if (group.isEmpty()) "" else " · turn $group"
        return "$who · $where · id=$rowId$groupSuffix"
    }

    companion object {
        const val KIND_USER = "user"
        const val KIND_ASSISTANT = "assistant"
        const val KIND_TOOL = "tool"
        const val KIND_APPROVAL = "approval"
        const val KIND_SYSTEM = "system"

        fun system(text: String): TranscriptRowPayload =
            TranscriptRowPayload(KIND_SYSTEM, text, -1, "", "")
    }
}

/**
 * The host clipboard with the existing permission story: the user presses
 * Copy (an explicit action, exactly like the attachment paste binding); the
 * write goes through the AWT system clipboard ONLY on a real display. A
 * headless JVM never touches the toolkit — tests inject [recorder].
 */
internal object HostClipboard {
    @Volatile
    var recorder: ((String) -> Unit)? = null

    /** Copies [text]; false when the host refuses (headless, no clipboard). */
    fun copy(text: String): Boolean {
        recorder?.let {
            it(text)
            return true
        }
        if (GraphicsEnvironment.isHeadless()) return false
        return try {
            val clipboard = Toolkit.getDefaultToolkit().systemClipboard
            clipboard.setContents(StringSelection(text), null)
            true
        } catch (e: Exception) {
            false
        }
    }
}

/**
 * Safe link navigation: only the [RichMarkdown.safeUrl] allowlist can ever
 * reach a host action, the scheme is re-validated AT CLICK time, and a
 * headless JVM (or a host without Desktop browse) is a typed no-op.
 */
internal object HostLinks {
    @Volatile
    var opener: ((String) -> Unit)? = null

    fun open(url: String): Boolean {
        val safe = RichMarkdown.safeUrl(url) ?: return false
        opener?.let {
            it(safe)
            return true
        }
        if (GraphicsEnvironment.isHeadless()) return false
        return try {
            if (!Desktop.isDesktopSupported()) return false
            val desktop = Desktop.getDesktop()
            if (!desktop.isSupported(Desktop.Action.BROWSE)) return false
            desktop.browse(URI(safe))
            true
        } catch (e: Exception) {
            false
        }
    }
}

// ---------------------------------------------------------------- blocks

/** One rendered markdown block component with in-place content refresh. */
internal interface MarkdownBlockView {
    val component: JComponent
    fun apply(block: MarkdownBlock)
}

private fun mono(base: Font): Font = monospacePanelFont().deriveFont(
    Font.PLAIN,
    base.size2D
)

private fun codeTint(): java.awt.Color =
    mix(panelSurface(), textForeground(), if (isDarkSurface()) 0.10f else 0.045f)

private fun linkColor(): java.awt.Color = FaktorTheme.accent()

/**
 * A styled text block (heading/paragraph/list/quote): a non-editable
 * wrapping JTextPane with explicit attribute sets — never HTML. Safe links
 * record their document range for tooltip/click handling.
 */
internal class RichTextPane(baseFont: Font) : JTextPane(), MarkdownBlockView {

    override val component: JComponent get() = this

    /** Document-range → safe URL (re-validated again at click time). */
    val links = ArrayList<Pair<IntRange, String>>()

    private var base = baseFont

    init {
        isEditable = false
        border = BorderFactory.createEmptyBorder(1, 0, 1, 0)
        background = panelSurface()
        foreground = textForeground()
        isOpaque = true
        font = baseFont
        selectionColor = FaktorTheme.accentTint(0.30f)
        // Link activation + tooltip: position → recorded range → re-validated
        // scheme. A click on anything outside the allowlist does nothing.
        addMouseListener(object : MouseAdapter() {
            override fun mouseClicked(event: MouseEvent) {
                val url = urlAt(event.x, event.y) ?: return
                HostLinks.open(url)
            }
        })
    }

    fun applyFont(font: Font) {
        base = font
    }

    private fun urlAt(x: Int, y: Int): String? {
        val position = viewToModel2D(java.awt.Point(x, y)).takeIf { it >= 0 } ?: return null
        for ((range, url) in links) {
            if (position in range) return url
        }
        return null
    }

    override fun getToolTipText(event: MouseEvent): String? {
        val url = urlAt(event.x, event.y) ?: return super.getToolTipText(event)
        return "opens $url (http/https/mailto only)"
    }

    override fun apply(block: MarkdownBlock) {
        val document = styledDocument
        document.remove(0, document.length)
        links.clear()
        when (block) {
            is MarkdownBlock.Heading -> {
                val level = block.level.coerceIn(1, 6)
                val scale = when (level) {
                    1 -> 1.35f
                    2 -> 1.22f
                    3 -> 1.10f
                    4 -> 1.04f
                    else -> 1.0f
                }
                val attributes = SimpleAttributeSet()
                StyleConstants.setBold(attributes, true)
                StyleConstants.setFontSize(attributes, (base.size2D * scale).toInt().coerceAtLeast(9))
                StyleConstants.setForeground(attributes, textForeground())
                appendSpans(document, attributes, block.spans)
            }
            is MarkdownBlock.Paragraph -> appendSpans(document, paragraphAttributes(0, 2), block.spans)
            is MarkdownBlock.Quote -> {
                val attributes = paragraphAttributes(18, 4)
                StyleConstants.setForeground(attributes, mutedForeground())
                for ((index, line) in block.lines.withIndex()) {
                    if (index > 0) document.insertString(document.length, "\n", null)
                    appendSpans(document, attributes, line)
                }
            }
            is MarkdownBlock.BulletList -> appendList(document, block.items, numberedFrom = null)
            is MarkdownBlock.NumberedList -> appendList(document, block.items, numberedFrom = block.startAt)
            is MarkdownBlock.Table -> {
                // Structure visible in monospace (plain-but-aligned).
                val attributes = SimpleAttributeSet()
                StyleConstants.setFontFamily(attributes, Font.MONOSPACED)
                StyleConstants.setFontSize(attributes, (base.size2D - 1f).toInt().coerceAtLeast(9))
                StyleConstants.setForeground(attributes, textForeground())
                document.insertString(0, renderTable(block), attributes)
            }
            is MarkdownBlock.Code -> {
                // A code block renders through its own component; a stray
                // apply here degrades to plain monospace text.
                val attributes = SimpleAttributeSet()
                StyleConstants.setFontFamily(attributes, Font.MONOSPACED)
                document.insertString(0, block.code, attributes)
            }
            is MarkdownBlock.Rule -> Unit
        }
    }

    private fun appendList(
        document: StyledDocument,
        items: List<ListItem>,
        numberedFrom: Int?
    ) {
        items.forEachIndexed { index, item ->
            if (index > 0) document.insertString(document.length, "\n", null)
            val prefix = if (numberedFrom == null) {
                "• "
            } else {
                "${numberedFrom + index}. "
            }
            val attributes = paragraphAttributes(12 * item.indent, 1)
            document.insertString(document.length, prefix, attributes)
            appendSpans(document, attributes, item.spans)
        }
    }

    private fun paragraphAttributes(indent: Int, @Suppress("UNUSED_PARAMETER") spaceAfter: Int): SimpleAttributeSet {
        val attributes = SimpleAttributeSet()
        StyleConstants.setFontFamily(attributes, base.family)
        StyleConstants.setFontSize(attributes, base.size)
        StyleConstants.setForeground(attributes, textForeground())
        if (indent > 0) {
            attributes.addAttribute(StyleConstants.LeftIndent, indent.toFloat())
        }
        return attributes
    }

    private fun appendSpans(
        document: StyledDocument,
        base: SimpleAttributeSet,
        spans: List<InlineSpan>
    ) {
        for (span in spans) {
            when (span) {
                is InlineSpan.Text -> document.insertString(document.length, span.text, base)
                is InlineSpan.Strong -> {
                    val attributes = SimpleAttributeSet(base)
                    StyleConstants.setBold(attributes, true)
                    appendSpans(document, attributes, span.spans)
                }
                is InlineSpan.Emphasis -> {
                    val attributes = SimpleAttributeSet(base)
                    StyleConstants.setItalic(attributes, true)
                    appendSpans(document, attributes, span.spans)
                }
                is InlineSpan.CodeSpan -> {
                    val attributes = SimpleAttributeSet(base)
                    StyleConstants.setFontFamily(attributes, Font.MONOSPACED)
                    StyleConstants.setBackground(attributes, codeTint())
                    document.insertString(document.length, span.text, attributes)
                }
                is InlineSpan.Link -> {
                    // Defense in depth: the pane RE-VALIDATES the scheme at
                    // render time — a hostile span can never become
                    // interactive even if it bypassed the parser.
                    val safe = RichMarkdown.safeUrl(span.url)
                    if (safe == null) {
                        document.insertString(document.length, span.label, base)
                    } else {
                        val attributes = SimpleAttributeSet(base)
                        StyleConstants.setUnderline(attributes, true)
                        StyleConstants.setForeground(attributes, linkColor())
                        val start = document.length
                        document.insertString(document.length, span.label, attributes)
                        val range = start until document.length
                        if (range.first < range.last) links.add(range to safe)
                    }
                }
            }
        }
    }
}

/** The monospace aligned table block (plain-but-aligned, bounded). */
internal class TableBlock(baseFont: Font) : JTextArea(), MarkdownBlockView {
    override val component: JComponent get() = this

    init {
        isEditable = false
        lineWrap = false
        font = mono(baseFont)
        foreground = textForeground()
        background = panelSurface()
        isOpaque = true
        border = BorderFactory.createEmptyBorder(2, 0, 2, 0)
        tabSize = 4
    }

    fun applyFont(font: Font) {
        this.font = mono(font)
    }

    override fun apply(block: MarkdownBlock) {
        val table = block as? MarkdownBlock.Table ?: return
        val text = renderTable(table)
        if (this.text != text) this.text = text
    }
}

/** One aligned monospace rendering of a table (shared by pane + block). */
internal fun renderTable(table: MarkdownBlock.Table): String {
    val columns = (table.header.size + table.rows.sumOf { it.size }).coerceAtLeast(1)
        .coerceAtMost(RichMarkdown.MAX_TABLE_COLUMNS)
    val widths = IntArray(columns)
    fun measure(row: List<String>) {
        for (column in 0 until minOf(row.size, columns)) {
            widths[column] = maxOf(widths[column], row[column].length.coerceAtMost(64))
        }
    }
    measure(table.header)
    table.rows.forEach { measure(it) }
    fun line(row: List<String>, pad: (Int) -> Char): String {
        val cells = ArrayList<String>(columns)
        for (column in 0 until columns) {
            val raw = row.getOrElse(column) { "" }.take(64)
            val width = widths[column]
            val padding = " ".repeat((width - raw.length).coerceAtLeast(0))
            val left = padding.length / 2
            cells.add(
                when (pad(column)) {
                    'R' -> padding + raw
                    'C' -> " ".repeat(left) + raw + " ".repeat(padding.length - left)
                    else -> raw + padding
                }
            )
        }
        return cells.joinToString("  ").trimEnd()
    }
    val out = StringBuilder()
    out.append(line(table.header) { column ->
        when (table.alignments.getOrNull(column)) {
            TableAlign.RIGHT -> 'R'
            TableAlign.CENTER -> 'C'
            else -> 'L'
        }
    }).append('\n')
    val rule = widths.joinToString("  ") { "-".repeat(maxOf(it, 1)) }
    out.append(rule)
    for (row in table.rows) {
        out.append('\n').append(line(row) { column ->
            when (table.alignments.getOrNull(column)) {
                TableAlign.RIGHT -> 'R'
                TableAlign.CENTER -> 'C'
                else -> 'L'
            }
        })
    }
    return out.toString()
}

/**
 * A fenced code block: monospace body, language label and the Copy action
 * (host clipboard, explicit user gesture, headless-guarded).
 */
internal class CodeBlockView(baseFont: Font) : JPanel(BorderLayout()), MarkdownBlockView {

    override val component: JComponent get() = this

    val languageLabel = JLabel()
    val copyButton = javax.swing.JButton("Copy")
    val codeArea = JTextArea()
    var code: String = ""
        private set

    private val base = baseFont

    init {
        isOpaque = true
        background = codeTint()
        border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(1, 0, 1, 0, FaktorTheme.separator()),
            EmptyBorder(Spacing.XS, Spacing.S, Spacing.S, Spacing.S)
        )
        val header = JPanel(FlowLayout(FlowLayout.LEFT, 6, 0))
        header.isOpaque = false
        languageLabel.font = mono(base).deriveFont(Font.BOLD)
        languageLabel.foreground = mutedForeground()
        languageLabel.a11y("Code block language", "The language label of the fenced code block.")
        copyButton.font = base.deriveFont(Font.PLAIN, base.size2D - 1f)
        copyButton.a11y("Copy code", "Copies this code block to the host clipboard.")
        copyButton.toolTipText = "Copy this code block to the clipboard"
        copyButton.addActionListener { copyCode() }
        header.add(languageLabel)
        header.add(Box.createHorizontalGlue())
        header.add(copyButton)
        add(header, BorderLayout.NORTH)
        codeArea.isEditable = false
        codeArea.lineWrap = true
        codeArea.wrapStyleWord = false
        codeArea.font = mono(base)
        codeArea.foreground = textForeground()
        codeArea.background = background
        codeArea.isOpaque = false
        codeArea.border = EmptyBorder(0, 0, 0, 0)
        codeArea.a11y("Code block", "Read-only code; select to copy, or press Copy.")
        add(codeArea, BorderLayout.CENTER)
    }

    fun applyFont(font: Font) {
        languageLabel.font = mono(font).deriveFont(Font.BOLD)
        copyButton.font = font.deriveFont(Font.PLAIN, font.size2D - 1f)
        codeArea.font = mono(font)
    }

    override fun apply(block: MarkdownBlock) {
        val codeBlock = block as? MarkdownBlock.Code ?: return
        code = codeBlock.code
        if (codeArea.text != code) codeArea.text = code
        val language = if (codeBlock.language.isEmpty()) "code" else codeBlock.language
        if (languageLabel.text != language) languageLabel.text = language
    }

    private fun copyCode() {
        val copied = HostClipboard.copy(code)
        val original = "Copy"
        copyButton.text = if (copied) "Copied" else "Unavailable"
        val timer = Timer(1_200) {
            copyButton.text = original
        }
        timer.isRepeats = false
        timer.start()
    }
}

/** Create the component that renders [block] (never HTML, never a widget bomb). */
internal fun createBlockView(block: MarkdownBlock, baseFont: Font): MarkdownBlockView {
    val view: MarkdownBlockView = when (block) {
        is MarkdownBlock.Heading -> RichTextPane(baseFont)
        is MarkdownBlock.Paragraph ->
            // Fast path: an all-literal paragraph still renders through the
            // same component so block identity rules stay uniform.
            RichTextPane(baseFont)
        is MarkdownBlock.Quote -> RichTextPane(baseFont)
        is MarkdownBlock.BulletList -> RichTextPane(baseFont)
        is MarkdownBlock.NumberedList -> RichTextPane(baseFont)
        is MarkdownBlock.Table -> TableBlock(baseFont)
        is MarkdownBlock.Code -> CodeBlockView(baseFont)
        is MarkdownBlock.Rule -> object : MarkdownBlockView {
            override val component: JComponent = JSeparator(JSeparator.HORIZONTAL)
                .apply { preferredSize = Dimension(0, 1) }
            override fun apply(block: MarkdownBlock) = Unit
        }
    }
    view.apply(block)
    return view
}

// ------------------------------------------------------- assistant body

/**
 * The incremental markdown body of ONE assistant row.
 *
 * Invariants (adversarially tested):
 *  - SETTLED blocks keep their Swing component identity across every later
 *    update — selection/focus in a completed block survives streaming;
 *  - an update re-renders only the TAIL (the unsettled suffix); while the
 *    tail keeps its block-type sequence, the tail components are refreshed
 *    IN PLACE (same instances);
 *  - when the tail itself settles, its single last-block component is kept
 *    in place (content refreshed once to the final source) and joins the
 *    settled list;
 *  - a non-append edit (durable divergence) rebuilds everything once — the
 *    model normally prevents it.
 */
internal class AssistantBody(baseFont: Font) : JPanel() {

    private class Rendered(val view: MarkdownBlockView, val typeKey: String) {
        val component: JComponent get() = view.component
    }

    private var base = baseFont
    private val done = ArrayList<Rendered>()
    private val tail = ArrayList<Rendered>()
    private var doneSource = ""
    private var doneEnd = 0
    private var tailTypes: List<String> = emptyList()

    var renderedText: String = ""
        private set
    var settleRenders: Int = 0
        private set
    var tailRenders: Int = 0
        private set

    init {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        alignmentX = 0f
        isOpaque = false
    }

    fun applyFont(font: Font) {
        base = font
        for (rendered in done + tail) {
            when (val view = rendered.view) {
                is RichTextPane -> view.applyFont(font)
                is TableBlock -> view.applyFont(font)
                is CodeBlockView -> view.applyFont(font)
                else -> Unit
            }
        }
    }

    fun settledComponentsForTest(): List<JComponent> = done.map { it.component }

    fun tailComponentsForTest(): List<JComponent> = tail.map { it.component }

    fun update(text: String) {
        if (text == renderedText) return
        if (!text.startsWith(doneSource)) {
            rebuildAll(text)
            return
        }
        val settled = RichMarkdown.settledEnd(text)
        if (settled > doneEnd) {
            settleRegion(text, settled)
        }
        renderTail(text.substring(doneEnd))
        renderedText = text
    }

    /** Full rebuild (first render, or a non-append divergence). */
    private fun rebuildAll(text: String) {
        removeAllFrom(done)
        removeAllFrom(tail)
        doneEnd = 0
        doneSource = ""
        tailTypes = emptyList()
        val settled = RichMarkdown.settledEnd(text)
        if (settled > 0) settleRegion(text, settled)
        renderTail(text.substring(doneEnd))
        renderedText = text
    }

    /**
     * Promote the completed region [doneEnd, settled) into settled blocks.
     * The dominant streaming case — a single tail block that just completed
     * — keeps that exact component instance (identity survives the settle).
     */
    private fun settleRegion(text: String, settled: Int) {
        val region = text.substring(doneEnd, settled)
        val blocks = RichMarkdown.parse(region)
        if (blocks.isEmpty()) {
            doneEnd = settled
            doneSource = text.substring(0, settled)
            return
        }
        val keepTail = if (tail.size == 1 && blocks.size == 1 &&
            tail[0].typeKey == typeKeyOf(blocks[0])
        ) {
            tail.removeAt(0)
        } else {
            removeAllFrom(tail)
            null
        }
        for ((index, block) in blocks.withIndex()) {
            if (keepTail != null && index == blocks.lastIndex) {
                keepTail.view.apply(block)
                done.add(keepTail)
                continue
            }
            val view = createBlockView(block, base)
            mount(view.component)
            done.add(Rendered(view, typeKeyOf(block)))
            settleRenders++
        }
        tailTypes = emptyList()
        doneEnd = settled
        doneSource = text.substring(0, settled)
    }

    /** Render the unsettled suffix; reuse tail components while types match. */
    private fun renderTail(source: String) {
        if (source.isEmpty()) {
            removeAllFrom(tail)
            tailTypes = emptyList()
            return
        }
        val blocks = RichMarkdown.parse(source)
        val types = blocks.map { typeKeyOf(it) }
        if (types != tailTypes || tail.size != blocks.size) {
            removeAllFrom(tail)
            tailTypes = types
            for (block in blocks) {
                val view = createBlockView(block, base)
                mount(view.component)
                tail.add(Rendered(view, typeKeyOf(block)))
                tailRenders++
            }
            return
        }
        // Same type sequence: refresh in place (the tail keeps identity).
        for ((index, block) in blocks.withIndex()) {
            tail[index].view.apply(block)
            tailRenders++
        }
    }

    private fun mount(component: JComponent) {
        component.alignmentX = 0f
        add(component)
    }

    private fun removeAllFrom(list: MutableList<Rendered>) {
        for (rendered in list) remove(rendered.component)
        list.clear()
    }

    private fun typeKeyOf(block: MarkdownBlock): String = when (block) {
        is MarkdownBlock.Heading -> "heading"
        is MarkdownBlock.Paragraph -> "paragraph"
        is MarkdownBlock.Quote -> "quote"
        is MarkdownBlock.BulletList -> "bullets"
        is MarkdownBlock.NumberedList -> "numbers"
        is MarkdownBlock.Table -> "table"
        is MarkdownBlock.Code -> "code"
        is MarkdownBlock.Rule -> "rule"
    }
}

// -------------------------------------------------------------- cell

/** One transcript row: optional identity header + kind-specific body. */
internal class TranscriptCell(
    val key: String,
    baseFont: Font
) : JPanel(BorderLayout(0, 2)) {

    val identityLabel = JLabel()
    var payload: TranscriptRowPayload = TranscriptRowPayload.system("")
        private set
    var body: JComponent = JTextArea()
        private set
    private var base = baseFont
    private var bodyKind: String = ""

    init {
        isOpaque = false
        alignmentX = 0f
        border = EmptyBorder(0, Spacing.M, Spacing.S, Spacing.M)
        identityLabel.font = baseFont.deriveFont(Font.BOLD, baseFont.size2D - 1f)
        identityLabel.border = EmptyBorder(0, 0, 1, 0)
        identityLabel.a11y("Transcript identity", "You or Faktor, once per turn.")
        identityLabel.isVisible = false
        add(identityLabel, BorderLayout.NORTH)
        buildBody(TranscriptRowPayload.KIND_SYSTEM)
    }

    fun applyFont(font: Font) {
        base = font
        identityLabel.font = font.deriveFont(Font.BOLD, font.size2D - 1f)
        when (val current = body) {
            is AssistantBody -> current.applyFont(font)
            is ToolBody -> {
                current.chip.font = font.deriveFont(Font.BOLD, font.size2D - 2f)
                current.area.font = mono(font)
            }
            is JTextArea -> current.font = font
            is CodeBlockView -> current.applyFont(font)
            is RichTextPane -> current.applyFont(font)
            is TableBlock -> current.applyFont(font)
            else -> Unit
        }
    }

    fun apply(newPayload: TranscriptRowPayload, showIdentity: Boolean) {
        if (newPayload.kind != bodyKind) buildBody(newPayload.kind)
        payload = newPayload
        when (val current = body) {
            is AssistantBody -> current.update(newPayload.text)
            is ToolBody -> {
                current.chip.text = newPayload.label ?: "tool"
                if (current.area.text != newPayload.text) current.area.text = newPayload.text
            }
            is JTextArea -> {
                if (current.text != newPayload.text) current.text = newPayload.text
            }
            else -> Unit
        }
        val identity = newPayload.identity()
        identityLabel.text = identity ?: ""
        identityLabel.foreground = if (identity == "Faktor") {
            FaktorTheme.accent()
        } else {
            textForeground()
        }
        identityLabel.isVisible = identity != null && showIdentity
        toolTipText = newPayload.diagnostic()
    }

    private fun buildBody(kind: String) {
        remove(body)
        bodyKind = kind
        body = when (kind) {
            TranscriptRowPayload.KIND_ASSISTANT -> AssistantBody(base)
            TranscriptRowPayload.KIND_TOOL -> toolBody()
            else -> JTextArea().apply {
                isEditable = false
                lineWrap = true
                wrapStyleWord = true
                font = base
                foreground = textForeground()
                background = panelSurface()
                isOpaque = true
                border = EmptyBorder(1, 0, 1, 0)
                a11y("Transcript row", "Read-only transcript text.")
            }
        }
        body.alignmentX = 0f
        add(body, BorderLayout.CENTER)
    }

    private fun toolBody(): JComponent {
        val chip = JLabel("tool")
        chip.font = base.deriveFont(Font.BOLD, base.size2D - 2f)
        chip.foreground = FaktorTheme.accent()
        chip.border = EmptyBorder(0, 0, 1, 0)
        chip.a11y("Tool part", "A discrete tool part of the turn.")
        val area = JTextArea().apply {
            isEditable = false
            lineWrap = true
            wrapStyleWord = false
            font = mono(base)
            foreground = textForeground()
            background = panelSurface()
            isOpaque = true
            border = EmptyBorder(1, 0, 1, 0)
            a11y("Tool output", "Read-only tool payload text.")
        }
        return ToolBody(chip, area)
    }

    /** A tool row: kind chip + monospace payload area. */
    class ToolBody(
        val chip: JLabel,
        val area: JTextArea
    ) : JPanel(BorderLayout(0, 0)) {
        init {
            isOpaque = false
            alignmentX = 0f
            add(chip, BorderLayout.NORTH)
            add(area, BorderLayout.CENTER)
        }
    }
}

// -------------------------------------------------------------- view

/**
 * The ordered, keyed host of every transcript row. Cell components are
 * stable per key; the identity header shows once per turn group (the first
 * row of a run of rows sharing [TranscriptRowPayload.group]).
 */
internal class TranscriptView : JPanel() {

    private val order = ArrayList<String>()
    private val cells = LinkedHashMap<String, TranscriptCell>()
    private var sysCounter = 0L
    private var approvalCounter = 0L
    private var baseFont: Font = uiPanelFont().deriveFont(13f)

    /** Cell mutations (adds/removes/in-place updates) — harness observable. */
    val mutations = java.util.concurrent.atomic.AtomicLong()

    init {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        isOpaque = true
        background = panelSurface()
        font = baseFont
        alignmentX = 0f
        a11y(
            "Conversation transcript",
            "Live conversation with You and Faktor identities."
        )
    }

    fun applyFont(font: Font) {
        baseFont = font
        this.font = font
        for (cell in cells.values) cell.applyFont(font)
    }

    fun contains(key: String): Boolean = cells.containsKey(key)

    fun payloadOf(key: String): TranscriptRowPayload? = cells[key]?.payload

    fun cellOf(key: String): TranscriptCell? = cells[key]

    fun rowCount(): Int = order.size

    fun keyAt(index: Int): String? = order.getOrNull(index)

    /** One unkeyed system/notice run cell (consecutive runs coalesce). */
    fun appendRun(text: String) {
        if (text.isEmpty()) return
        insertCell("sys:${sysCounter++}", TranscriptRowPayload.system(text), order.size)
    }

    /** One approval part (interleaves with turn parts by arrival order). */
    fun appendPart(payload: TranscriptRowPayload) {
        insertCell(
            "ap:${approvalCounter++}",
            payload.copy(seq = approvalCounter - 1, rowId = "part-${approvalCounter - 1}"),
            order.size
        )
    }

    fun upsert(key: String, payload: TranscriptRowPayload, beforeKey: String?) {
        val existing = cells[key]
        if (existing != null) {
            val index = order.indexOf(key)
            if (existing.payload != payload) {
                existing.apply(payload, isGroupStart(index))
                mutations.incrementAndGet()
                // A group change can flip the NEIGHBOURS' identity headers
                // (first-of-turn visibility depends on the previous row).
                refreshIdentityAround(index)
            }
            return
        }
        if (key.startsWith("sys:") || key.startsWith("ap:")) return
        insertCell(key, payload, anchorIndex(beforeKey))
    }

    fun rekey(oldKey: String, newKey: String, payload: TranscriptRowPayload, beforeKey: String?) {
        removeCell(oldKey)
        upsert(newKey, payload, beforeKey)
    }

    /** One removed row (the bounded-trim primitive reports its identity). */
    data class RemovedRow(val key: String, val payload: TranscriptRowPayload)

    /** Removes the oldest row (the bounded-trim primitive). */
    fun removeOldest(): RemovedRow? {
        val key = order.firstOrNull() ?: return null
        val payload = cells[key]?.payload ?: return null
        removeCell(key)
        return RemovedRow(key, payload)
    }

    fun clear() {
        for (key in order.toList()) removeCell(key)
        order.clear()
        cells.clear()
    }

    /** The plain-text projection used by smokes/diagnostics. */
    fun plainText(): String {
        val out = StringBuilder()
        for (index in order.indices) {
            val cell = cells[order[index]] ?: continue
            out.append(cell.payload.plainText(isGroupStart(index)))
            out.append('\n')
        }
        return out.toString()
    }

    /** Scrolls one row into view (message navigation; headless-safe). */
    fun scrollTo(key: String) {
        val cell = cells[key] ?: return
        try {
            cell.scrollRectToVisible(cell.bounds)
        } catch (e: Exception) {
            // Not realized yet: focus is best-effort, never a failure.
        }
    }

    private fun anchorIndex(beforeKey: String?): Int {
        if (beforeKey == null) return order.size
        val index = order.indexOf(beforeKey)
        return if (index < 0) order.size else index
    }

    private fun insertCell(key: String, payload: TranscriptRowPayload, index: Int) {
        val cell = TranscriptCell(key, baseFont)
        cell.apply(payload, showIdentity = false)
        val at = index.coerceIn(0, order.size)
        order.add(at, key)
        cells[key] = cell
        add(cell, at)
        mutations.incrementAndGet()
        refreshIdentityAround(at)
    }

    private fun removeCell(key: String) {
        val index = order.indexOf(key)
        if (index < 0) return
        val cell = cells.remove(key) ?: return
        order.removeAt(index)
        remove(cell)
        mutations.incrementAndGet()
        refreshIdentityAround(index)
    }

    private fun refreshIdentityAround(index: Int) {
        if (order.isEmpty()) return
        for (i in maxOf(0, index - 1)..minOf(order.lastIndex, maxOf(index, 0))) {
            val cell = cells[order[i]] ?: continue
            cell.apply(cell.payload, isGroupStart(i))
        }
    }

    /** True when this row starts its turn group (identity header shows). */
    fun isGroupStart(index: Int): Boolean {
        val cell = cells[order.getOrNull(index) ?: return false] ?: return false
        if (cell.payload.identity() == null) return false
        if (index <= 0) return true
        val previous = cells[order[index - 1]] ?: return true
        return previous.payload.group != cell.payload.group
    }
}
