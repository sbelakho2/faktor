// Small shared Swing helpers for the Faktor panels (no third-party deps).
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import java.awt.Font
import javax.swing.BorderFactory
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextArea

/** A read-only, wrapped, monospace text area sized for compact payloads. */
internal fun compactArea(rows: Int, cols: Int = 36): JTextArea {
    val area = JTextArea(rows, cols)
    area.isEditable = false
    area.lineWrap = true
    area.wrapStyleWord = true
    area.font = Font(Font.MONOSPACED, Font.PLAIN, 12)
    return area
}

/** A titled vertical section with a bordered body. */
internal fun titledSection(title: String, body: Component): JPanel {
    val panel = JPanel(BorderLayout(0, 2))
    panel.border = BorderFactory.createEmptyBorder(4, 6, 4, 6)
    panel.add(JLabel(title).apply { font = font.deriveFont(Font.BOLD) }, BorderLayout.NORTH)
    panel.add(body, BorderLayout.CENTER)
    return panel
}

/** Wraps a component in a scroll pane with a vertical-only policy default. */
internal fun scroll(component: Component): JScrollPane = JScrollPane(component)

private val DISPLAY_CONTROL_CHARS = Regex(
    "[\\u0000-\\u0008\\u000B\\u000C\\u000E-\\u001F\\u007F-\\u009F" +
        "\\u200E\\u200F\\u202A-\\u202E\\u2066-\\u2069]"
)

/**
 * Bounds a display string to [max] chars with an ellipsis marker. UTF-16
 * truncation never splits a surrogate pair (a cut mid-pair renders as
 * U+FFFD), and invisible C0/C1 controls plus explicit bidi embedding/
 * override controls are stripped: they are layout/spoofing vectors, never
 * human text. RTL letters and shaping are untouched.
 */
/** take(max) that never splits a surrogate pair at the cut. */
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
