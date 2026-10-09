// Small shared Swing helpers for the Faktor panels (no third-party deps).
//
// This file owns the plugin's tiny visual system: a 4/8/12/16 spacing scale,
// cards with subtle theme-token borders and a normal-case semibold header,
// muted secondary text, status chips, consistently sized actions and
// width-aware wrapped text. Every color is resolved from the active
// UIManager/LAF tokens (with contrast-derived fallbacks) so the panels follow
// IDE themes and HiDPI/zoom settings instead of a hardcoded palette.
package dev.faktor.frontend

import java.awt.BasicStroke
import java.awt.BorderLayout
import java.awt.Color
import java.awt.Component
import java.awt.Dimension
import java.awt.Font
import java.awt.Graphics
import java.awt.Graphics2D
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Insets
import java.awt.RenderingHints
import java.awt.event.ComponentAdapter
import java.awt.event.ComponentEvent
import java.awt.event.FocusAdapter
import java.awt.event.FocusEvent
import java.awt.event.MouseAdapter
import java.awt.event.MouseEvent
import java.awt.event.MouseMotionAdapter
import java.awt.font.FontRenderContext
import javax.swing.BorderFactory
import javax.swing.Box
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JTabbedPane
import javax.swing.JTable
import javax.swing.JTextArea
import javax.swing.JToggleButton
import javax.swing.UIManager
import javax.swing.border.AbstractBorder
import javax.swing.text.JTextComponent

/** The shared 4/8/12/16 spacing scale (px at 100% IDE zoom). */
internal object Spacing {
    const val XS = 4
    const val S = 8
    const val M = 12
    const val L = 16
}

/** The shared corner radii: cards read as one system at 8px. */
internal object Radii {
    const val CARD = 8
    const val CONTROL = 6
    const val CHIP = 10
}

/**
 * The Faktor accent. ONE restrained identity hue — a deep engineering teal,
 * deliberately distinct from IntelliJ blue and from generic assistant
 * palettes — used only for primary actions, selected states, focus rings and
 * the active cluster tab underline. Everything else stays platform-native. A
 * theme that registers `Faktor.accent` in UIManager overrides the fixed hue;
 * otherwise the fixed teal is tone-adjusted for the active light/dark surface.
 */
internal object FaktorTheme {

    /** The live accent (deep teal `#0C737D` light / `#2FB8AB` dark by default). */
    fun accent(): Color = UIManager.getColor("Faktor.accent") ?: if (isDarkSurface()) {
        Color(0x2F, 0xB8, 0xAB)
    } else {
        Color(0x0C, 0x73, 0x7D)
    }

    /** The accent under the pointer (perceptibly lifted, never a flash). */
    fun accentHover(): Color = towardContrast(accent(), 0.12f)

    /** The accent while pressed (deeper than hover). */
    fun accentPressed(): Color = towardContrast(accent(), 0.26f)

    /** Legible text on an accent fill (near-black on bright teal, else white). */
    fun onAccent(): Color = if (luminance(accent()) > 0.45) {
        Color(0x0B, 0x14, 0x13)
    } else {
        Color.WHITE
    }

    /** A 0..1 accent tint over the panel surface (selected fills, focus halos). */
    fun accentTint(strength: Float): Color = mix(panelSurface(), accent(), strength)

    /** The subtle rollover background for rows and secondary controls. */
    fun hover(): Color = mix(
        panelSurface(),
        textForeground(),
        if (isDarkSurface()) 0.10f else 0.055f
    )

    /** The pressed background for secondary controls. */
    fun pressed(): Color = mix(
        panelSurface(),
        textForeground(),
        if (isDarkSurface()) 0.18f else 0.10f
    )

    /** The slightly raised card surface (never a hardcoded palette entry). */
    fun cardSurface(): Color = mix(
        panelSurface(),
        textForeground(),
        if (isDarkSurface()) 0.045f else 0.018f
    )

    /** The hairline separating a header row from its body. */
    fun separator(): Color = mix(cardBorderColor(), panelSurface(), 0.35f)

    /** The hairline of an inset scroll surface nested inside a card. */
    fun insetBorder(): Color = mix(cardBorderColor(), cardSurface(), 0.45f)

    private fun towardContrast(base: Color, amount: Float): Color =
        if (isDarkSurface()) mix(base, Color.WHITE, amount) else mix(base, Color.BLACK, amount)
}

/**
 * One rounded 1px outline (optionally filled) on the shared radius scale.
 * Resolves its colors at construction from the active theme tokens; painted
 * with antialiasing so cards and chips read deliberate at every IDE zoom.
 */
internal class FaktorRoundedBorder(
    private val lineColor: Color,
    private val radius: Int = Radii.CARD,
    private val thickness: Int = 1,
    private val fill: Color? = null,
    private val padding: Insets = Insets(0, 0, 0, 0)
) : AbstractBorder() {

    override fun getBorderInsets(c: Component?): Insets =
        Insets(padding.top, padding.left, padding.bottom, padding.right)

    override fun getBorderInsets(c: Component?, insets: Insets): Insets {
        insets.set(padding.top, padding.left, padding.bottom, padding.right)
        return insets
    }

    override fun isBorderOpaque(): Boolean = false

    override fun paintBorder(c: Component?, g: Graphics, x: Int, y: Int, w: Int, h: Int) {
        val g2 = g.create() as Graphics2D
        try {
            g2.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            val arc = radius * 2
            val fillColor = fill
            if (fillColor != null) {
                g2.color = fillColor
                g2.fillRoundRect(x, y, w - 1, h - 1, arc, arc)
            }
            if (thickness > 0) {
                g2.color = lineColor
                g2.stroke = BasicStroke(thickness.toFloat())
                g2.drawRoundRect(x, y, w - 1, h - 1, arc, arc)
            }
        } finally {
            g2.dispose()
        }
    }
}

/** One vertical gap in a BoxLayout column (the shared spacing scale). */
internal fun vSpace(px: Int = Spacing.S): Component = Box.createVerticalStrut(px)

/**
 * The active IDE/LAF UI font. Ordinary panel text derives from it instead of
 * a hardcoded family/size, so IDE zoom, HiDPI scaling and user font settings
 * flow through; the platform label font is the honest fallback.
 */
internal fun uiPanelFont(): Font =
    UIManager.getFont("Label.font")
        ?: JLabel().font
        ?: Font(Font.SANS_SERIF, Font.PLAIN, 12)

/**
 * Monospace typography for code/output/commands/hashes (explicit opt-in).
 * The IDE editor font is used when the active UIManager exposes one (IntelliJ
 * sets `TextArea.font` to the editor font); otherwise the platform monospaced
 * family is derived at the UI font size.
 */
internal fun monospacePanelFont(): Font {
    val base = uiPanelFont()
    val editor = UIManager.getFont("TextArea.font")
    if (editor != null && isMonospaced(editor)) return editor
    return Font(Font.MONOSPACED, Font.PLAIN, base.size)
}

private fun isMonospaced(font: Font): Boolean {
    val context = FontRenderContext(null, false, false)
    val narrow = font.getStringBounds("i", context).width
    val wide = font.getStringBounds("W", context).width
    return Math.abs(narrow - wide) < 0.5
}

/** The semibold face used for card/section headers and primary actions. */
internal fun sectionTitleFont(): Font = uiPanelFont().deriveFont(Font.BOLD)

/** The active label foreground (the primary text color). */
internal fun textForeground(): Color =
    UIManager.getColor("Label.foreground")
        ?: JLabel().foreground
        ?: Color(0x33, 0x33, 0x33)

/** The active panel surface every card and page sits on. */
internal fun panelSurface(): Color =
    UIManager.getColor("Panel.background")
        ?: JPanel().background
        ?: Color(0xF2, 0xF2, 0xF2)

/**
 * Muted secondary text: the IDE's disabled-foreground token when the theme
 * exposes one, otherwise the normal foreground blended toward the panel
 * surface so it stays legible in every light/dark/high-contrast theme.
 */
internal fun mutedForeground(): Color {
    for (key in MUTED_KEYS) {
        val color = UIManager.getColor(key)
        if (color != null) return color
    }
    return mix(textForeground(), panelSurface(), 0.42f)
}

private val MUTED_KEYS = arrayOf(
    "Label.disabledForeground",
    "Component.disabledForeground"
)

/**
 * The subtle card outline: the IDE's own border token when registered,
 * otherwise a low-contrast blend of the foreground into the panel surface
 * (a hint of structure, never a heavy box).
 */
internal fun cardBorderColor(): Color {
    for (key in arrayOf("Component.borderColor", "Separator.separatorColor", "controlShadow")) {
        val color = UIManager.getColor(key)
        if (color != null) return color
    }
    return mix(textForeground(), panelSurface(), 0.76f)
}

/** Linear blend of two colors (t = 0 keeps [from], t = 1 returns [to]). */
internal fun mix(from: Color, to: Color, t: Float): Color {
    val clamped = t.coerceIn(0f, 1f)
    fun channel(a: Int, b: Int): Int =
        (a * (1 - clamped) + b * clamped).toInt().coerceIn(0, 255)
    return Color(channel(from.red, to.red), channel(from.green, to.green), channel(from.blue, to.blue))
}

/** The active label foreground is light: a dark IDE/LAF surface. */
internal fun isDarkSurface(): Boolean {
    val foreground = UIManager.getColor("Label.foreground") ?: return false
    return luminance(foreground) > 0.5
}

/** Relative luminance of one color (WCAG weights). */
internal fun luminance(color: Color): Double {
    fun channel(value: Int): Double {
        val v = value / 255.0
        return if (v <= 0.03928) v / 12.92 else Math.pow((v + 0.055) / 1.055, 2.4)
    }
    return 0.2126 * channel(color.red) +
        0.7152 * channel(color.green) +
        0.0722 * channel(color.blue)
}

/**
 * A read-only, wrapped text area sized for compact payloads. Ordinary prose
 * uses the IDE UI font; [monospace] is the explicit opt-in reserved for
 * code/output/commands/hashes.
 */
internal fun compactArea(rows: Int, cols: Int = 36, monospace: Boolean = false): JTextArea {
    val area = JTextArea(rows, cols)
    area.isEditable = false
    area.lineWrap = true
    area.wrapStyleWord = true
    area.font = if (monospace) monospacePanelFont() else uiPanelFont()
    return area
}

/** Strip invisible C0/C1 controls plus bidi embedding/override controls. */
private val DISPLAY_CONTROL_CHARS = Regex(
    "[\\u0000-\\u0008\\u000B\\u000C\\u000E-\\u001F\\u007F-\\u009F" +
        "\\u200E\\u200F\\u202A-\\u202E\\u2066-\\u2069]"
)

/** Escape a plain display string for Swing's HTML 3.2 renderer. */
internal fun escapeHtml(text: String): String = text
    .replace("&", "&amp;")
    .replace("<", "&lt;")
    .replace(">", "&gt;")

/**
 * Insert zero-width break opportunities after path/identifier separators so
 * a long path or hash wraps instead of clipping (Swing's HTML renderer breaks
 * at U+200B). Newlines become `<br>`.
 */
internal fun breakableText(text: String): String {
    val sb = StringBuilder(text.length + 16)
    for (ch in text) {
        when (ch) {
            '\n' -> sb.append("<br>")
            '/', '\\', '.', '_', '-', ',', ';', ':', '|' -> {
                sb.append(ch).append('\u200B')
            }
            else -> sb.append(ch)
        }
    }
    return sb.toString()
}

/**
 * A width-aware wrapped label: the text is rendered as escaped HTML with a
 * real pixel width hint, recomputed whenever the label is resized. Long
 * unbroken tokens (paths, hashes) get zero-width break opportunities. The
 * untruncated text stays available as the tooltip.
 */
internal class WrappedLabel(
    text: String = "",
    private val preferredWidthCap: Int = Int.MAX_VALUE
) : JLabel() {

    private var wrappedWidth = -1

    private var rendering = false

    var fullText: String = ""
        set(value) {
            field = DISPLAY_CONTROL_CHARS.replace(value, "")
            wrappedWidth = -1
            refresh()
        }

    init {
        font = uiPanelFont()
        foreground = textForeground()
        verticalAlignment = TOP
        alignmentX = Component.LEFT_ALIGNMENT
        addComponentListener(
            object : ComponentAdapter() {
                override fun componentResized(event: ComponentEvent?) {
                    refresh()
                }
            }
        )
        fullText = text
    }

    /**
     * AWT posts component-resized events through the EventQueue, so an
     * offscreen/one-pass layout would render stale text. Re-wrap synchronously
     * at the exact moment the layout manager assigns the width; the posted
     * event remains a harmless second notification.
     */
    override fun setBounds(x: Int, y: Int, w: Int, h: Int) {
        super.setBounds(x, y, w, h)
        refresh()
    }

    override fun setSize(dimension: Dimension?) {
        super.setSize(dimension)
        refresh()
    }

    /**
     * Existing `label.text = ...` call sites keep working: an external text
     * assignment sets the UNWRAPPED text, which is re-wrapped on resize.
     */
    override fun setText(value: String?) {
        if (rendering) {
            super.setText(value)
        } else {
            fullText = value ?: ""
        }
    }

    /**
     * A wrapped label must be allowed to shrink below the fallback wrap
     * width, otherwise GridBag/BorderLayout clip it at narrow widths instead
     * of allocating the real column width (and triggering a re-wrap).
     */
    override fun getMinimumSize(): Dimension {
        val base = super.getMinimumSize()
        return Dimension(Spacing.L * 4, base.height)
    }

    /**
     * The HTML preferred width over-reports the visual width; a caller can
     * cap it so a label column does not swallow the whole form.
     */
    override fun getPreferredSize(): Dimension {
        val base = super.getPreferredSize()
        return Dimension(Math.min(base.width, preferredWidthCap), base.height)
    }

    /**
     * Re-wraps at the current width. Before the first layout pass a
     * conservative 220px hint is used; the synchronous [setBounds] re-wrap
     * replaces it with the real width.
     *
     * The lines are broken here with the label's own font metrics and joined
     * with `<br>`, so the rendered text can never be clipped by Swing's HTML
     * body-margin/width-style arithmetic (the old `style='width:...'` hint
     * under-measured the margins and cut words at the right edge at 240px).
     */
    private fun refresh() {
        val available = if (width > 0) width else 220
        if (available == wrappedWidth) return
        wrappedWidth = available
        val maxWidth = (available - Spacing.L - Spacing.XS).coerceAtLeast(40)
        val metrics = getFontMetrics(font ?: uiPanelFont())
        val lines = wrapLines(fullText, maxWidth) { candidate -> metrics.stringWidth(candidate) }
        rendering = true
        try {
            super.setText("<html>" + lines.joinToString("<br>") { escapeHtml(it) } + "</html>")
        } finally {
            rendering = false
        }
        toolTipText = fullText.ifEmpty { null }
    }

    companion object {
        /**
         * Greedy wrap at spaces and after path/identifier separators, with a
         * hard character cut only when one glyph run is wider than the whole
         * line. Pure, so it is exercised without a display.
         */
        internal fun wrapLines(
            text: String,
            maxWidth: Int,
            widthOf: (String) -> Int
        ): List<String> {
            val lines = ArrayList<String>()
            for (hard in text.split('\n')) {
                if (hard.isEmpty()) {
                    lines.add("")
                    continue
                }
                var start = 0
                var lastBreak = -1
                var i = 0
                while (i < hard.length) {
                    val ch = hard[i]
                    if (isBreakAfter(ch)) lastBreak = i + 1
                    val candidate = hard.substring(start, i + 1)
                    if (candidate.isNotEmpty() && widthOf(candidate) > maxWidth && lastBreak > start) {
                        lines.add(hard.substring(start, lastBreak).trimEnd())
                        start = lastBreak
                        i = start
                        lastBreak = -1
                        continue
                    }
                    i++
                }
                lines.add(hard.substring(start).trimEnd())
            }
            return lines
        }

        private fun isBreakAfter(ch: Char): Boolean = when (ch) {
            ' ', '\u200B', '/', '\\', '.', '-', '_', ',', ';', ':', '|' -> true
            else -> false
        }
    }
}

/**
 * Top-down page layout: every child keeps its preferred height (stacked in
 * order) and only the width is forced to the container. Unlike BoxLayout it
 * never stretches a child to fill leftover space (no giant voids, no blown-up
 * rows) and never caches stale requirements when a wrapped card grows.
 */
internal class PageLayout : java.awt.LayoutManager {
    override fun addLayoutComponent(name: String?, comp: Component?) = Unit

    override fun removeLayoutComponent(comp: Component?) = Unit

    override fun preferredLayoutSize(parent: java.awt.Container): Dimension {
        val insets = parent.insets
        var width = 0
        var height = 0
        for (child in parent.components) {
            if (!child.isVisible) continue
            val size = child.preferredSize
            width = Math.max(width, size.width)
            height += size.height
        }
        return Dimension(width + insets.left + insets.right, height + insets.top + insets.bottom)
    }

    override fun minimumLayoutSize(parent: java.awt.Container): Dimension = preferredLayoutSize(parent)

    override fun layoutContainer(parent: java.awt.Container) {
        val insets = parent.insets
        val width = (parent.width - insets.left - insets.right).coerceAtLeast(0)
        var y = insets.top
        for (child in parent.components) {
            if (!child.isVisible) continue
            val height = child.preferredSize.height
            child.setBounds(insets.left, y, width, height)
            y += height
        }
    }
}

/**
 * A vertical card/page column that tracks the scroll viewport width (no
 * horizontal scrollbar at 240px: wrapped content reflows instead) and scrolls
 * by 16px units (wheel/HiDPI friendly).
 */
internal open class ScrollableColumn : JPanel(), javax.swing.Scrollable {
    init {
        layout = PageLayout()
        isOpaque = true
        background = panelSurface()
    }

    override fun getPreferredScrollableViewportSize(): Dimension = preferredSize

    override fun getScrollableUnitIncrement(
        visibleRect: java.awt.Rectangle?,
        orientation: Int,
        direction: Int
    ): Int = Spacing.L

    override fun getScrollableBlockIncrement(
        visibleRect: java.awt.Rectangle?,
        orientation: Int,
        direction: Int
    ): Int = 64

    override fun getScrollableTracksViewportWidth(): Boolean = true

    override fun getScrollableTracksViewportHeight(): Boolean = false
}

/**
 * A form-field label wrapped at a FIXED width: the break points never depend
 * on how much column the layout manager assigns, so the label column cannot
 * creep narrower pass after pass (the feedback loop that used to shred
 * "Verification" into "Verifi catio n" at wide widths). Long unbroken words
 * simply overflow into the (empty) gap before the field.
 */
internal class FixedWrapLabel(text: String, private val wrapWidth: Int = Spacing.L * 8) : JLabel() {
    init {
        font = uiPanelFont()
        foreground = mutedForeground()
        verticalAlignment = TOP
        val metrics = getFontMetrics(font)
        val lines = WrappedLabel.wrapLines(text, (wrapWidth - Spacing.L).coerceAtLeast(24)) {
            metrics.stringWidth(it)
        }
        super.setText("<html>" + lines.joinToString("<br>") { escapeHtml(it) } + "</html>")
        toolTipText = text
    }

    override fun getPreferredSize(): Dimension {
        val base = super.getPreferredSize()
        return Dimension(Math.min(base.width, wrapWidth + Spacing.XS), base.height)
    }

    override fun getMinimumSize(): Dimension = preferredSize
}

/**
 * A scroll pane for a tall page column: a short tool window scrolls (16px
 * wheel units) instead of clipping the last cards out of reach. The page
 * column already tracks the viewport width, so no horizontal scrollbar is
 * needed; the scrollbar only appears when the content overflows.
 */
internal fun pageScroll(content: JComponent): javax.swing.JScrollPane =
    javax.swing.JScrollPane(content).apply {
        border = null
        verticalScrollBar.unitIncrement = Spacing.L
        horizontalScrollBarPolicy =
            javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
    }

/**
 * A page/card body column on the shared spacing scale: vertical BoxLayout
 * with uniform outer padding (the "no giant gray voids, consistent outer
 * padding" rule). Children are stacked in order; callers add [vSpace] gaps.
 */
internal fun pageColumn(gap: Int = Spacing.S, padding: Int = Spacing.M): JPanel {
    val panel = ScrollableColumn()
    panel.border = BorderFactory.createEmptyBorder(padding, padding, padding, padding)
    panel.putClientProperty("faktor.gap", gap)
    return panel
}

/**
 * The card factory: one raised 8px-radius surface with an optional
 * normal-case semibold header and 12px inner padding. The rounded fill and
 * the 1px hairline outline are painted from live theme tokens (never raw
 * RGB), so cards read as one product under light, dark and high-contrast
 * themes. The panel itself is non-opaque; the shared border owns the shape.
 */
internal fun card(
    title: String?,
    body: Component,
    hgap: Int = 0,
    vgap: Int = if (title == null) 0 else Spacing.S
): JPanel {
    val panel = JPanel(BorderLayout(hgap, vgap))
    panel.isOpaque = false
    panel.background = FaktorTheme.cardSurface()
    panel.alignmentX = Component.LEFT_ALIGNMENT
    panel.border = FaktorRoundedBorder(
        lineColor = cardBorderColor(),
        radius = Radii.CARD,
        fill = FaktorTheme.cardSurface(),
        padding = Insets(Spacing.M, Spacing.M, Spacing.M, Spacing.M)
    )
    if (title != null) {
        // Width-aware: a long card title wraps at 240px instead of being
        // ellipsized ("Mutation mode (Task c...)").
        val header = WrappedLabel(title).apply {
            font = sectionTitleFont()
            foreground = textForeground()
        }
        panel.add(header, BorderLayout.NORTH)
    }
    panel.add(body, BorderLayout.CENTER)
    return panel
}

/** A titled vertical section with a bordered body (card factory shorthand). */
internal fun titledSection(title: String, body: Component): JPanel = card(title, body)

/** A standalone card/section header in the normal-case semibold face. */
internal fun sectionHeader(text: String, muted: Boolean = false): WrappedLabel =
    WrappedLabel(text).apply {
        font = sectionTitleFont()
        foreground = if (muted) mutedForeground() else textForeground()
        alignmentX = Component.LEFT_ALIGNMENT
    }

/** Muted secondary text (hints, counts, empty states). */
internal fun mutedLabel(text: String): JLabel =
    JLabel(text).apply {
        font = uiPanelFont()
        foreground = mutedForeground()
        alignmentX = Component.LEFT_ALIGNMENT
    }

/** A width-aware muted hint that wraps instead of clipping at 240px. */
internal fun wrappedMutedLabel(text: String): WrappedLabel =
    WrappedLabel(text).apply {
        font = uiPanelFont()
        foreground = mutedForeground()
        alignmentX = Component.LEFT_ALIGNMENT
    }

/** A semantic state that the UI must keep legible under every IDE theme. */
internal enum class SemanticState { POSITIVE, NEGATIVE, WARNING, DIM }

/**
 * Theme-derived semantic foregrounds. IntelliJ registers its `Objects.*` and
 * disabled-foreground tokens with UIManager, so the panels consume the live
 * theme instead of raw RGB; plain Swing/LAF hosts fall back to
 * contrast-aware values derived from the current label foreground. State is
 * NEVER carried by color alone: call sites also render an explicit
 * pass/fail/unavailable marker or label.
 */
internal fun semanticForeground(state: SemanticState): Color {
    val keys = when (state) {
        SemanticState.POSITIVE -> arrayOf("Objects.Green")
        SemanticState.NEGATIVE -> arrayOf("Objects.Red")
        SemanticState.WARNING -> arrayOf("Objects.Yellow")
        SemanticState.DIM -> arrayOf("Label.disabledForeground", "Component.disabledForeground")
    }
    for (key in keys) {
        val color = UIManager.getColor(key)
        if (color != null) return color
    }
    val dark = isDarkSurface()
    return when (state) {
        SemanticState.POSITIVE ->
            if (dark) Color(0x7A, 0xD9, 0x8A) else Color(0x1B, 0x6E, 0x2F)
        SemanticState.NEGATIVE ->
            if (dark) Color(0xFF, 0x8A, 0x8A) else Color(0xA6, 0x2A, 0x2A)
        SemanticState.WARNING ->
            if (dark) Color(0xE6, 0xC3, 0x66) else Color(0x7A, 0x5A, 0x00)
        SemanticState.DIM ->
            if (dark) Color(0x9E, 0x9E, 0x9E) else Color(0x8C, 0x8C, 0x8C)
    }
}

/**
 * A status chip: the state word plus a subtle rounded pill in the theme's
 * semantic tone, on a faint tone-tinted fill. The text carries the state; the
 * color only re-enforces it.
 */
internal fun statusChip(text: String, state: SemanticState): JLabel {
    val tone = semanticForeground(state)
    return JLabel(text).apply {
        font = sectionTitleFont()
        foreground = tone
        isOpaque = false
        border = FaktorRoundedBorder(
            lineColor = tone,
            radius = Radii.CHIP,
            fill = mix(panelSurface(), tone, 0.10f),
            padding = Insets(1, Spacing.S, 1, Spacing.S)
        )
    }
}

/**
 * A consistently sized themed action: rounded fill with a real rollover and
 * pressed state, the shared UI font, and a 28px primary / 24px secondary
 * height floor that still grows with IDE zoom (the height derives from the
 * resolved font metrics). [primary] actions carry the one accent hue of the
 * product; secondary actions stay surface-toned with a hairline outline, so
 * emphasis is unambiguous. Destructive actions stay secondary by call site.
 */
internal class FaktorButton(text: String, private val primary: Boolean = false) : JButton(text) {

    init {
        isFocusPainted = false
        isBorderPainted = false
        isContentAreaFilled = false
        isOpaque = false
        isRolloverEnabled = true
        font = if (primary) sectionTitleFont() else uiPanelFont()
        border = BorderFactory.createEmptyBorder(verticalPad(), horizontalPad(), verticalPad(), horizontalPad())
    }

    private fun verticalPad(): Int = if (primary) 6 else 4

    private fun horizontalPad(): Int = if (primary) 14 else 10

    private fun heightFloor(): Int = if (primary) 28 else 24

    override fun getPreferredSize(): Dimension {
        val base = super.getPreferredSize()
        return Dimension(base.width, Math.max(heightFloor(), base.height))
    }

    override fun paintComponent(g: Graphics) {
        val g2 = g.create() as Graphics2D
        try {
            g2.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            val arc = Radii.CONTROL * 2
            val w = width
            val h = height
            val fill: Color
            val outline: Color?
            when {
                !isEnabled -> {
                    fill = panelSurface()
                    outline = mix(cardBorderColor(), panelSurface(), 0.4f)
                }
                primary -> {
                    fill = when {
                        model.isPressed -> FaktorTheme.accentPressed()
                        model.isRollover -> FaktorTheme.accentHover()
                        else -> FaktorTheme.accent()
                    }
                    outline = null
                }
                else -> {
                    fill = when {
                        model.isPressed -> FaktorTheme.pressed()
                        model.isRollover -> FaktorTheme.hover()
                        else -> panelSurface()
                    }
                    outline = cardBorderColor()
                }
            }
            g2.color = fill
            g2.fillRoundRect(0, 0, w - 1, h - 1, arc, arc)
            if (outline != null) {
                g2.color = outline
                g2.drawRoundRect(0, 0, w - 1, h - 1, arc, arc)
            }
            if (isEnabled && (hasFocus() || isFocusOwner)) {
                g2.color = FaktorTheme.accent()
                g2.stroke = BasicStroke(1.4f)
                g2.drawRoundRect(1, 1, w - 3, h - 3, arc, arc)
            }
        } finally {
            g2.dispose()
        }
        val previous = foreground
        foreground = when {
            !isEnabled -> mutedForeground()
            primary -> FaktorTheme.onAccent()
            else -> textForeground()
        }
        try {
            super.paintComponent(g)
        } finally {
            foreground = previous
        }
    }
}

/** A themed toggle (the inspector switch): selected state carries the accent. */
internal class FaktorToggleButton(text: String) : JToggleButton(text) {

    init {
        isFocusPainted = false
        isBorderPainted = false
        isContentAreaFilled = false
        isOpaque = false
        isRolloverEnabled = true
        font = uiPanelFont()
        border = BorderFactory.createEmptyBorder(4, 10, 4, 10)
    }

    override fun getPreferredSize(): Dimension {
        val base = super.getPreferredSize()
        return Dimension(base.width, Math.max(24, base.height))
    }

    override fun paintComponent(g: Graphics) {
        val g2 = g.create() as Graphics2D
        try {
            g2.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            val arc = Radii.CONTROL * 2
            val w = width
            val h = height
            val fill: Color
            val outline: Color
            when {
                !isEnabled -> {
                    fill = panelSurface()
                    outline = mix(cardBorderColor(), panelSurface(), 0.4f)
                }
                isSelected -> {
                    fill = FaktorTheme.accentTint(0.18f)
                    outline = FaktorTheme.accent()
                }
                model.isPressed -> {
                    fill = FaktorTheme.pressed()
                    outline = cardBorderColor()
                }
                model.isRollover -> {
                    fill = FaktorTheme.hover()
                    outline = cardBorderColor()
                }
                else -> {
                    fill = panelSurface()
                    outline = cardBorderColor()
                }
            }
            g2.color = fill
            g2.fillRoundRect(0, 0, w - 1, h - 1, arc, arc)
            g2.color = outline
            g2.drawRoundRect(0, 0, w - 1, h - 1, arc, arc)
            if (isEnabled && (hasFocus() || isFocusOwner)) {
                g2.color = FaktorTheme.accent()
                g2.stroke = BasicStroke(1.4f)
                g2.drawRoundRect(1, 1, w - 3, h - 3, arc, arc)
            }
        } finally {
            g2.dispose()
        }
        val previous = foreground
        foreground = when {
            !isEnabled -> mutedForeground()
            isSelected -> FaktorTheme.accent()
            else -> textForeground()
        }
        try {
            super.paintComponent(g)
        } finally {
            foreground = previous
        }
    }
}

/** The affirmative action of a card (accent fill, 28px minimum height). */
internal fun primaryButton(text: String): JButton = FaktorButton(text, primary = true)

/** A supporting action (surface fill, hairline outline, 24px minimum height). */
internal fun secondaryButton(text: String): JButton = FaktorButton(text, primary = false)

/**
 * Kept for call sites that only need the shared font/margin conventions; the
 * themed [FaktorButton] is the concrete class in every panel.
 */
internal fun actionButton(text: String, primary: Boolean = false): JButton =
    FaktorButton(text, primary = primary)

/**
 * A compact two-column form grid: muted field labels in the west column,
 * fields filling east. Rows are on the 8px grid; [span] adds a full-width
 * row (areas, hints).
 */
internal class FormGrid {

    private val panel = JPanel(GridBagLayout())

    private var row = 0

    init {
        panel.isOpaque = false
        panel.background = panelSurface()
    }

    private fun constraints(): GridBagConstraints = GridBagConstraints().apply {
        insets = Insets(0, 0, 6, Spacing.S)
        anchor = GridBagConstraints.WEST
        fill = GridBagConstraints.HORIZONTAL
        gridy = row
    }

    fun row(labelText: String, field: JComponent): FormGrid {
        val label = FixedWrapLabel(labelText).apply {
            labelFor = field
        }
        // A text field's default minimum size equals its preferred size, and
        // GridBagLayout only distributes negative space down to minimum
        // sizes: at 240px tool windows the east column used to overflow and
        // clip the input. Allowing the field to shrink keeps the form inside
        // the card at every host width (the label column stays wrapped).
        field.minimumSize = Dimension(
            Spacing.L * 3,
            field.minimumSize.height
        )
        panel.add(label, constraints().apply {
            gridx = 0
            weightx = 0.0
        })
        panel.add(field, constraints().apply {
            gridx = 1
            weightx = 1.0
        })
        row++
        return this
    }

    fun span(component: JComponent): FormGrid {
        panel.add(component, constraints().apply {
            gridx = 0
            gridwidth = 2
            weightx = 1.0
        })
        row++
        return this
    }

    /** A west column without a field (checkbox rows, hints). */
    fun west(component: JComponent): FormGrid {
        panel.add(component, constraints().apply {
            gridx = 0
            gridwidth = 2
            weightx = 1.0
        })
        row++
        return this
    }

    fun build(): JPanel = panel
}

/** One horizontal action row on the shared spacing scale. */
internal fun actionRow(vararg buttons: JButton): JPanel {
    val panel = ActionRowPanel()
    panel.layout = java.awt.FlowLayout(java.awt.FlowLayout.LEFT, Spacing.S, Spacing.XS)
    panel.isOpaque = false
    for (button in buttons) panel.add(button)
    return panel
}

/**
 * A left-aligned button row that WRAPS when the host narrows (240px tool
 * window) instead of clipping the trailing controls out of reach. The
 * preferred height is computed for the width the page layout assigns —
 * `FlowLayout` performs the actual row wrapping — so the column reserves the
 * wrapped rows' full height.
 */
private class ActionRowPanel : JPanel() {
    override fun getPreferredSize(): Dimension {
        val natural = super.getPreferredSize()
        val target = if (width > 0) width else parent?.width ?: 0
        if (target <= 0 || natural.width <= target) return natural
        val layout = layout as? java.awt.FlowLayout ?: return natural
        val rowHeight = (natural.height - 2 * layout.vgap).coerceAtLeast(1)
        var rows = 1
        var x = insets.left
        for (child in components) {
            if (!child.isVisible) continue
            val childWidth = child.preferredSize.width
            if (x > insets.left &&
                x + childWidth + layout.hgap > target - insets.right
            ) {
                rows++
                x = insets.left
            }
            x += childWidth + layout.hgap
        }
        return Dimension(natural.width, natural.height + (rows - 1) * (rowHeight + layout.vgap))
    }
}

/**
 * The inset scroll surface nested inside a card: the card outline is the only
 * frame, so the inner viewport gets one rounded hairline (no double square
 * border) and the viewport keeps its own surface through the rounded corners.
 * Rows carry their full text in a tooltip instead of forcing sideways panning
 * at 240px (no horizontal scrollbar).
 */
internal fun insetScroll(view: Component): javax.swing.JScrollPane =
    javax.swing.JScrollPane(view).apply {
        isOpaque = false
        border = FaktorRoundedBorder(FaktorTheme.insetBorder(), radius = Radii.CONTROL)
        verticalScrollBar.unitIncrement = Spacing.L
        horizontalScrollBarPolicy = javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
    }

/**
 * An inset scroll for a bare table that also installs the column header
 * explicitly (offscreen renders never run `JTable.addNotify`).
 */
internal fun insetTableScroll(table: javax.swing.JTable): javax.swing.JScrollPane {
    val scroll = insetScroll(table)
    val header = table.tableHeader
    if (header != null) scroll.setColumnHeaderView(header)
    return scroll
}

/**
 * The chat input surface: one rounded hairline that turns into the accent
 * focus ring while the text component owns focus (keyboard affordance).
 */
internal fun focusAccentScroll(view: JTextComponent): javax.swing.JScrollPane {
    val scroll = javax.swing.JScrollPane(view)
    scroll.isOpaque = false
    scroll.border = FaktorRoundedBorder(FaktorTheme.insetBorder(), radius = Radii.CONTROL)
    scroll.verticalScrollBar.unitIncrement = Spacing.L
    view.addFocusListener(
        object : FocusAdapter() {
            override fun focusGained(e: FocusEvent?) {
                scroll.border = FaktorRoundedBorder(FaktorTheme.accent(), radius = Radii.CONTROL)
            }

            override fun focusLost(e: FocusEvent?) {
                scroll.border = FaktorRoundedBorder(FaktorTheme.insetBorder(), radius = Radii.CONTROL)
            }
        }
    )
    return scroll
}

/**
 * The header strip of a panel: the sentence-case semibold readout over a
 * hairline separator, on the shared 12px gutter. The caller keeps the label
 * (it is width-aware and updated in place).
 */
internal fun panelHeader(label: JLabel): JPanel {
    label.font = sectionTitleFont()
    label.foreground = textForeground()
    val row = JPanel(BorderLayout())
    row.isOpaque = true
    row.background = panelSurface()
    row.border = BorderFactory.createCompoundBorder(
        BorderFactory.createMatteBorder(0, 0, 1, 0, FaktorTheme.separator()),
        BorderFactory.createEmptyBorder(Spacing.S + 2, Spacing.M, Spacing.S, Spacing.M)
    )
    row.add(label, BorderLayout.CENTER)
    return row
}

/**
 * The cluster tab strip: the platform tab rendering plus ONE Faktor cue — a
 * 2px accent underline under the active tab. Nothing else about the LAF tabs
 * is overridden, so IDE themes keep their native tab shapes.
 */
internal class FaktorTabbedPane : JTabbedPane() {

    override fun paintComponent(g: Graphics) {
        super.paintComponent(g)
        val index = selectedIndex
        if (index < 0) return
        val bounds = getBoundsAt(index) ?: return
        if (bounds.height <= 4) return
        val g2 = g.create() as Graphics2D
        try {
            g2.setRenderingHint(RenderingHints.KEY_ANTIALIASING, RenderingHints.VALUE_ANTIALIAS_ON)
            g2.color = FaktorTheme.accent()
            val inset = 4
            val width = (bounds.width - inset * 2).coerceAtLeast(2)
            g2.fillRoundRect(
                bounds.x + inset,
                bounds.y + bounds.height - 3,
                width,
                2,
                2,
                2
            )
        } finally {
            g2.dispose()
        }
    }
}

/**
 * The shared list-row rhythm and hover tracking: a 24px minimum row height
 * (recomputed when IDE zoom changes the list font) and one rollover
 * background for the row under the pointer, so every list reads as the same
 * control. Selection keeps the platform selection colors untouched.
 */
internal object RowRhythm {

    private const val HOVER_KEY = "faktor.hoverRowIndex"

    /** Applies the row rhythm and the rollover tracker to one list. */
    fun install(list: JList<*>) {
        applyHeight(list)
        list.addPropertyChangeListener("font") { applyHeight(list) }
        list.addMouseMotionListener(
            object : MouseMotionAdapter() {
                override fun mouseMoved(e: MouseEvent?) {
                    val point = e?.point ?: return
                    val index = list.locationToIndex(point)
                    val bounds = list.getCellBounds(index, index)
                    val inside = bounds != null && bounds.contains(point)
                    setHovered(list, if (inside) index else -1)
                }
            }
        )
        list.addMouseListener(
            object : MouseAdapter() {
                override fun mouseExited(e: MouseEvent?) {
                    setHovered(list, -1)
                }
            }
        )
    }

    /** The rollover background of [index], or null when it is not hovered. */
    fun hoverBackground(list: JList<*>?, index: Int): Color? {
        if (list == null || index < 0) return null
        val hovered = list.getClientProperty(HOVER_KEY) as? Int ?: -1
        return if (hovered == index) FaktorTheme.hover() else null
    }

    private fun applyHeight(list: JList<*>) {
        val metrics = list.getFontMetrics(list.font ?: uiPanelFont())
        list.fixedCellHeight = Math.max(24, metrics.height + 8)
    }

    private fun setHovered(list: JList<*>, index: Int) {
        val current = list.getClientProperty(HOVER_KEY) as? Int ?: -1
        if (current == index) return
        list.putClientProperty(HOVER_KEY, index)
        list.repaint()
    }
}

/**
 * The table twin of [RowRhythm]: one 24px minimum row height and the same
 * rollover tracking, so a candidates table breathes like every Faktor list.
 */
internal object TableRhythm {

    private const val HOVER_KEY = "faktor.hoverTableRow"

    fun install(table: javax.swing.JTable) {
        val metrics = table.getFontMetrics(table.font ?: uiPanelFont())
        table.rowHeight = Math.max(24, metrics.height + 8)
        table.addMouseMotionListener(
            object : MouseMotionAdapter() {
                override fun mouseMoved(e: MouseEvent?) {
                    val point = e?.point ?: return
                    val row = table.rowAtPoint(point)
                    setHovered(table, row)
                }
            }
        )
        table.addMouseListener(
            object : MouseAdapter() {
                override fun mouseExited(e: MouseEvent?) {
                    setHovered(table, -1)
                }
            }
        )
    }

    fun isHovered(table: JTable, row: Int): Boolean =
        row >= 0 && (table.getClientProperty(HOVER_KEY) as? Int ?: -1) == row

    private fun setHovered(table: javax.swing.JTable, row: Int) {
        val current = table.getClientProperty(HOVER_KEY) as? Int ?: -1
        if (current == row) return
        table.putClientProperty(HOVER_KEY, row)
        table.repaint()
    }
}

/**
 * take(max) that never splits a surrogate pair at the cut.
 */
internal fun safeTake(text: String, max: Int): String {
    if (text.length <= max) return text
    var end = max.coerceAtLeast(0)
    if (end > 0 && end < text.length &&
        text[end - 1].isHighSurrogate() && text[end].isLowSurrogate()
    ) {
        end -= 1
    }
    return text.substring(0, end)
}

/** takeLast(max) that never splits a surrogate pair at the front cut. */
internal fun safeTakeLast(text: String, max: Int): String {
    if (text.length <= max) return text
    val cut = (text.length - max).coerceAtLeast(0)
    var start = cut
    if (start > 0 && start < text.length &&
        text[start - 1].isHighSurrogate() && text[start].isLowSurrogate()
    ) {
        start += 1
    }
    return text.substring(start)
}

/** Count-aware plural for user-visible labels: never "1 session(s)". */
internal fun plural(count: Int, singular: String, pluralForm: String = singular + "s"): String =
    "$count " + if (count == 1) singular else pluralForm

internal fun bound(text: String?, max: Int): String {
    val clean = DISPLAY_CONTROL_CHARS.replace(text ?: "", "")
    if (clean.length <= max) return clean
    var end = max.coerceAtLeast(0)
    if (end > 0 && end < clean.length &&
        clean[end - 1].isHighSurrogate() && clean[end].isLowSurrogate()
    ) {
        end -= 1
    }
    return clean.substring(0, end) + "..."
}
