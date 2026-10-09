// The session/task status plane: the daemon + stream readout plus the task
// projection (state, active model, active tool, queue depth, usage,
// verification, changed files and durable index coverage). Pure presentation:
// every setter receives an already-computed full line from FaktorChatPanel's
// refreshers (`daemon: ...`, `state: ...`), so the panel never derives state
// of its own and the same component renders in the IDE tool window and the
// offscreen host matrix.
package dev.faktor.frontend

import dev.faktor.shared.NativeProjection
import java.awt.BorderLayout
import javax.swing.JPanel

class StatusPanel : JPanel(BorderLayout()) {

    private val daemonLabel = WrappedLabel("daemon: stopped")

    private val streamLabel = WrappedLabel("stream: off")

    private val stateLabel = WrappedLabel("state: -")

    private val modelLabel = WrappedLabel("model: -")

    private val toolLabel = WrappedLabel("active tool: -")

    private val queuedLabel = WrappedLabel("queued: 0")

    private val usageLabel = WrappedLabel("usage: -")

    private val verificationLabel = WrappedLabel("verification: -")

    private val filesLabel = WrappedLabel("files changed: 0")

    private val indexLabel = WrappedLabel("index: -")

    init {
        val statusBody = pageColumn(gap = Spacing.XS, padding = 0)
        for (label in listOf(
            daemonLabel, streamLabel, stateLabel, modelLabel, toolLabel,
            queuedLabel, usageLabel, verificationLabel, filesLabel, indexLabel
        )) {
            statusBody.add(label)
        }
        val page = pageColumn()
        page.add(card("Session status", statusBody))
        add(pageScroll(page), BorderLayout.CENTER)
    }

    fun setDaemon(text: String) {
        daemonLabel.text = text
    }

    fun setStream(text: String) {
        streamLabel.text = text
    }

    fun setState(text: String) {
        stateLabel.text = text
    }

    fun setModel(text: String) {
        modelLabel.text = text
    }

    fun setTool(text: String) {
        toolLabel.text = text
    }

    fun setQueued(text: String) {
        queuedLabel.text = text
    }

    fun setFiles(text: String) {
        filesLabel.text = text
    }

    fun setUsage(text: String) {
        usageLabel.text = text
    }

    fun setVerification(text: String) {
        verificationLabel.text = text
    }

    /** Durable index coverage (audits 5/6): a PARTIAL/capped round is named. */
    fun setIndex(text: String) {
        indexLabel.text = text
    }

    /** Applies one durable projection row set to the status column. */
    fun applyProjection(projection: NativeProjection) {
        stateLabel.text = "state: ${projection.machine} (${projection.label})"
        val active = projection.activeModel
        modelLabel.text = if (active == null) {
            "model: ${projection.provider}/${projection.model}"
        } else {
            "model: ${active.provider}/${active.model}" +
                (if (active.variant == null) "" else " (${active.variant})")
        }
        val tool = projection.activeTool
        toolLabel.text = if (tool == null) "active tool: -" else "active tool: ${tool.tool} [${tool.status}]"
        queuedLabel.text = "queued: ${projection.queued}"
        filesLabel.text = "files changed: ${projection.filesChanged.size}"
    }
}
