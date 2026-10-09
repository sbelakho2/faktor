// The session/task status plane: the daemon + stream readout plus the task
// projection (state, active model, active tool, queue depth, usage,
// verification, changed files and durable index coverage). Pure presentation:
// every setter receives an already-computed full line from FaktorChatPanel's
// refreshers (`daemon: ...`, `state: ...`), so the panel never derives state
// of its own and the same component renders in the IDE tool window and the
// offscreen host matrix. Each line renders as a muted key column plus a
// width-aware value column, so the readout scans as a definition list.
package dev.faktor.frontend

import dev.faktor.shared.NativeProjection
import java.awt.BorderLayout
import java.awt.Dimension
import javax.swing.JLabel
import javax.swing.JPanel

class StatusPanel : JPanel(BorderLayout()) {

    /** One `key:` / value row of the readout, aligned across the card. */
    private class StatusRow : JPanel(BorderLayout(Spacing.S, 0)) {
        val key = JLabel(" ")
        val value = WrappedLabel(" ")

        init {
            isOpaque = false
            key.font = uiPanelFont()
            key.foreground = mutedForeground()
            key.verticalAlignment = javax.swing.SwingConstants.TOP
            value.font = uiPanelFont()
            value.foreground = textForeground()
            add(key, BorderLayout.WEST)
            add(value, BorderLayout.CENTER)
        }

        fun apply(text: String) {
            val separator = text.indexOf(": ")
            if (separator <= 0) {
                key.text = ""
                value.fullText = text
            } else {
                key.text = text.substring(0, separator) + ":"
                value.fullText = text.substring(separator + 2)
            }
        }
    }

    private val daemonRow = StatusRow().apply { apply("daemon: stopped") }

    private val streamRow = StatusRow().apply { apply("stream: off") }

    private val stateRow = StatusRow().apply { apply("state: -") }

    private val modelRow = StatusRow().apply { apply("model: -") }

    private val toolRow = StatusRow().apply { apply("active tool: -") }

    private val queuedRow = StatusRow().apply { apply("queued: 0") }

    private val usageRow = StatusRow().apply { apply("usage: -") }

    private val verificationRow = StatusRow().apply { apply("verification: -") }

    private val filesRow = StatusRow().apply { apply("files changed: 0") }

    private val indexRow = StatusRow().apply { apply("index: -") }

    private val rows = listOf(
        daemonRow, streamRow, stateRow, modelRow, toolRow,
        queuedRow, usageRow, verificationRow, filesRow, indexRow
    )

    init {
        // One key column width across every row: the value texts start on the
        // same x, so the readout reads as a deliberate grid instead of ragged
        // "key: value" prose.
        var keyWidth = 0
        for (row in rows) {
            keyWidth = Math.max(keyWidth, row.key.preferredSize.width)
        }
        val statusBody = pageColumn(gap = 6, padding = 0)
        for ((index, row) in rows.withIndex()) {
            if (index > 0) statusBody.add(vSpace(6))
            row.key.preferredSize = Dimension(keyWidth, row.key.preferredSize.height)
            row.key.minimumSize = row.key.preferredSize
            statusBody.add(row)
        }
        val page = pageColumn()
        page.add(card("Session status", statusBody))
        add(pageScroll(page), BorderLayout.CENTER)
    }

    fun setDaemon(text: String) {
        daemonRow.apply(text)
    }

    fun setStream(text: String) {
        streamRow.apply(text)
    }

    fun setState(text: String) {
        stateRow.apply(text)
    }

    fun setModel(text: String) {
        modelRow.apply(text)
    }

    fun setTool(text: String) {
        toolRow.apply(text)
    }

    fun setQueued(text: String) {
        queuedRow.apply(text)
    }

    fun setFiles(text: String) {
        filesRow.apply(text)
    }

    fun setUsage(text: String) {
        usageRow.apply(text)
    }

    fun setVerification(text: String) {
        verificationRow.apply(text)
    }

    /** Durable index coverage (audits 5/6): a PARTIAL/capped round is named. */
    fun setIndex(text: String) {
        indexRow.apply(text)
    }

    /** Applies one durable projection row set to the status column. */
    fun applyProjection(projection: NativeProjection) {
        stateRow.apply("state: ${projection.machine} (${projection.label})")
        val active = projection.activeModel
        val model = if (active == null) {
            "model: ${projection.provider}/${projection.model}"
        } else {
            "model: ${active.provider}/${active.model}" +
                (if (active.variant == null) "" else " (${active.variant})")
        }
        modelRow.apply(model)
        val tool = projection.activeTool
        toolRow.apply(if (tool == null) "active tool: -" else "active tool: ${tool.tool} [${tool.status}]")
        queuedRow.apply("queued: ${projection.queued}")
        filesRow.apply("files changed: ${projection.filesChanged.size}")
    }
}
