// Lightweight assistant-text presentation for the native Swing transcript.
//
// The transcript is a plain `JTextArea` (no HTML, no JCEF), so "styled" here
// means host-appropriate STRUCTURE in the text itself, exactly like an IDE
// console:
//
//  - fenced code blocks become an indented bordered-ish block (┌/│/└) with an
//    optional language label; the code bytes themselves are untouched;
//  - ATX headings (# … ######) become a ▸/› prefixed line (no raw hashes);
//  - list bullets become • (indentation preserved);
//  - block quotes become │ lines;
//  - thematic breaks become a bounded rule;
//  - inline emphasis markers are removed and code spans keep their text.
//
// It never emits HTML/SVG and never interprets links: a hostile message can
// only ever contribute plain characters. Pure and bounded (line length is
// capped by the caller's transcript budget).
package dev.faktor.frontend

internal object AssistantMarkdown {
    private val HEADING = Regex("^(#{1,6})\\s+(.*)$")
    private val BULLET = Regex("^(\\s*)[-*+]\\s+(.*)$")
    private val QUOTE = Regex("^\\s*>\\s?(.*)$")
    private val RULE = Regex("^\\s*([-*_])(\\s*\\1){2,}\\s*$")
    private val FENCE = Regex("^\\s*```\\s*([A-Za-z0-9_+.-]*)\\s*$")
    private val INLINE_EMPHASIS = Regex("(\\*\\*|__)(.*?)\\1")

    fun format(text: String): String {
        if (text.isEmpty()) return text
        if (!text.contains('#') && !text.contains('`') && !text.contains('-') &&
            !text.contains('*') && !text.contains('>') && !text.contains('_')
        ) {
            return text
        }
        val lines = text.split('\n')
        val out = StringBuilder(text.length + 16)
        var inFence = false
        var fenceLanguage = ""
        for ((index, raw) in lines.withIndex()) {
            if (index > 0) out.append('\n')
            val fence = FENCE.matchEntire(raw)
            if (!inFence && fence != null) {
                inFence = true
                fenceLanguage = fence.groupValues[1]
                out.append("┌─ ")
                out.append(if (fenceLanguage.isEmpty()) "code" else "code (" + fenceLanguage + ")")
                continue
            }
            if (inFence) {
                if (fence != null) {
                    inFence = false
                    out.append("└─")
                    continue
                }
                out.append("│ ")
                out.append(raw)
                continue
            }
            val heading = HEADING.matchEntire(raw)
            if (heading != null) {
                val level = heading.groupValues[1].length
                val prefix = if (level <= 3) "▸ " else "› "
                out.append(prefix)
                out.append(inline(heading.groupValues[2]))
                continue
            }
            if (RULE.matches(raw)) {
                out.append("────────")
                continue
            }
            val bullet = BULLET.matchEntire(raw)
            if (bullet != null) {
                out.append(bullet.groupValues[1])
                out.append("• ")
                out.append(inline(bullet.groupValues[2]))
                continue
            }
            val quote = QUOTE.matchEntire(raw)
            if (quote != null) {
                out.append("│ ")
                out.append(inline(quote.groupValues[1]))
                continue
            }
            out.append(inline(raw))
        }
        return out.toString()
    }

    /** Strip inline emphasis/code markers; never interpret a link. */
    private fun inline(text: String): String {
        if (text.isEmpty()) return text
        var value = INLINE_EMPHASIS.replace(text) { match -> match.groupValues[2] }
        if (value.indexOf('`') >= 0) value = value.replace("`", "")
        return value
    }
}
