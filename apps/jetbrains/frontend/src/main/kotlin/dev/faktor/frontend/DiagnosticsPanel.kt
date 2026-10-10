// Diagnostics: the session/run readout as muted key/value rows (daemon,
// live updates, state, model, active tool, queue, usage, verification,
// changed files, durable index coverage) plus the explicit service controls
// — reconnect the live-update stream from its exact cursor and restart the
// service. This is the home of the daemon/stream/cursor machinery that the
// History surface deliberately does not carry. Pure presentation: every
// setter receives an already-computed full line from FaktorChatPanel's
// refreshers (`daemon: ...`, `state: ...`); the panel never derives state of
// its own. Raw ids/hashes stay reachable here, never on a primary surface.
package dev.faktor.frontend

import dev.faktor.shared.NativeProjection
import java.awt.BorderLayout
import javax.swing.JLabel
import javax.swing.JPanel

class DiagnosticsPanel : JPanel(BorderLayout()) {

    /**
     * One readout row: the muted key stacks above the wrapping value so no
     * fixed key column can squeeze the value to a sliver at 240px.
     */
    private class StatusRow : JPanel(BorderLayout(0, 2)) {
        val key = JLabel(" ")
        val value = WrappedLabel(" ")

        init {
            isOpaque = false
            key.font = uiPanelFont()
            key.foreground = mutedForeground()
            key.verticalAlignment = javax.swing.SwingConstants.TOP
            value.font = uiPanelFont()
            value.foreground = textForeground()
            add(key, BorderLayout.NORTH)
            add(value, BorderLayout.CENTER)
        }

        fun apply(text: String) {
            val separator = text.indexOf(": ")
            if (separator <= 0) {
                key.text = " "
                value.fullText = text
            } else {
                key.text = text.substring(0, separator)
                value.fullText = text.substring(separator + 2)
            }
        }
    }

    interface Listener {
        fun onRestart()
        fun onReconnect()
        fun onRefresh()
    }

    private val daemonRow = StatusRow().apply { apply("daemon: stopped") }

    private val streamRow = StatusRow().apply { apply("live updates: off") }

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

    private val restartButton = secondaryButton("Restart service")

    private val reconnectButton = secondaryButton("Reconnect stream")

    private val refreshButton = secondaryButton("Refresh diagnostics")

    private var listener: Listener? = null

    private var available = true

    init {
        val statusBody = pageColumn(gap = 6, padding = 0)
        for ((index, row) in rows.withIndex()) {
            if (index > 0) statusBody.add(vSpace(6))
            statusBody.add(row)
        }
        restartButton.addActionListener { listener?.onRestart() }
        reconnectButton.addActionListener { listener?.onReconnect() }
        refreshButton.addActionListener { listener?.onRefresh() }
        val connectionBody = pageColumn(gap = Spacing.XS, padding = 0)
        connectionBody.add(
            wrappedMutedLabel(
                "Reconnect resumes the live-update stream from its exact cursor; " +
                    "restart adopts a restarted service without duplicating journal frames."
            )
        )
        connectionBody.add(vSpace(Spacing.XS))
        connectionBody.add(actionRow(reconnectButton, restartButton, refreshButton))
        val page = pageColumn()
        page.add(card("Session and service", statusBody))
        page.add(vSpace(Spacing.M))
        page.add(card("Recovery", connectionBody))
        add(panelHeader(WrappedLabel("diagnostics")), BorderLayout.NORTH)
        add(pageScroll(page), BorderLayout.CENTER)
    }

    fun setListener(value: Listener?) {
        listener = value
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

    /** The connection readout: daemon description, live-update state, cursor. */
    fun setConnection(daemon: String, stream: String, cursor: Long, sessionId: String?) {
        daemonRow.apply("daemon: $daemon")
        streamRow.apply(
            "live updates: $stream at cursor $cursor" +
                (if (sessionId == null) "" else " for this conversation")
        )
    }

    fun setUnavailable(detailText: String) {
        available = false
        daemonRow.apply("daemon: unavailable")
        streamRow.apply("live updates: unavailable ($detailText)")
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

    fun restartEnabled(): Boolean = restartButton.isEnabled

    fun reconnectEnabled(): Boolean = reconnectButton.isEnabled

    fun available(): Boolean = available

    fun streamText(): String = streamRow.value.fullText

    fun daemonText(): String = daemonRow.value.fullText
}
