// Diagnostics: the session/run readout as muted key/value rows (service
// state, readiness, live updates with the last cursor, run state, model,
// active tool, queue, usage, verification, changed files, durable index
// coverage and the resolved daemon binary trust) plus the explicit recovery
// states (T27 reconnecting / T28 paused — verbatim copy, actions wired to
// the existing reconnect / snapshot-refresh routes, cursor preserved, no
// skip) and the sandbox setup guidance card (T32; guidance only, no write
// control). This is the home of the daemon/stream/cursor machinery that the
// History surface deliberately does not carry. Pure presentation: every
// setter receives an already-computed full line from FaktorChatPanel's
// refreshers; the panel never derives state of its own. Raw ids/hashes stay
// reachable here, never on a primary surface.
package dev.faktor.frontend

import dev.faktor.backend.DaemonBinary
import dev.faktor.shared.NativeProjection
import java.awt.BorderLayout
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JPanel

/** The three stream-recovery states the Recovery card can render. */
enum class StreamRecoveryState { NONE, RECONNECTING, PAUSED }

/**
 * D14/D10 trust wording for the resolved daemon executable. The bare
 * `<binary>.sha256` record yields exactly these labels; the word "signed"
 * never describes the bare checksum file (signed-install wording stays with
 * the launcher-scoped install layout).
 */
internal fun daemonTrustLabel(trust: DaemonBinary.Trust?): String = when (trust) {
    is DaemonBinary.Trust.Verified -> "Matches recorded checksum"
    is DaemonBinary.Trust.Unverified -> "No checksum record"
    is DaemonBinary.Trust.Refused -> "Checksum mismatch"
    null -> "—"
}

/** T32 copy (state matrix, verbatim). */
internal const val SANDBOX_SETUP_HEADLINE = "Shell execution needs a decision"

/** T32 supporting copy (verbatim). */
internal const val SANDBOX_SETUP_COPY = "The secure default remains fail-closed on this platform."

/** T32 action label (verbatim). */
internal const val SANDBOX_SETUP_ACTION = "Open setup guidance"

/** T27 copy (state matrix, verbatim). */
internal const val RECONNECTING_HEADLINE = "Reconnecting live updates"

internal const val RECONNECTING_COPY = "The last valid journal position is kept."

/** T28 copy (state matrix, verbatim). */
internal const val STREAM_PAUSED_HEADLINE = "Live updates paused"

internal const val STREAM_PAUSED_COPY =
    "The client cannot read the next durable event. Nothing is skipped."

/**
 * The exact CLI guidance command (selectable, copyable). Guidance only: no
 * UI write control and no UI-only bypass exists for the sandbox shell
 * decision; the CLI is the only authority that records it.
 */
internal const val SANDBOX_GUIDANCE_COMMAND = "faktor sandbox choose"

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

        fun applyKey(keyText: String, valueText: String) {
            key.text = keyText
            value.fullText = valueText
        }
    }

    interface Listener {
        fun onRestart()
        fun onReconnect()
        fun onRefresh()

        /** T27 `Wait`: the explicit reconnect already preserves the cursor. */
        fun onWaitForReconnect()

        /** T28 `Refresh from snapshot`: re-read the durable state, no skip. */
        fun onRefreshFromSnapshot()
    }

    private val serviceRow = StatusRow().apply { applyKey("Service", "—") }

    private val readinessRow = StatusRow().apply { applyKey("Readiness", "—") }

    private val streamRow = StatusRow().apply { applyKey("Live updates", "off") }

    private val stateRow = StatusRow().apply { applyKey("State", "—") }

    private val modelRow = StatusRow().apply { applyKey("Model", "—") }

    private val toolRow = StatusRow().apply { applyKey("Active tool", "—") }

    private val queuedRow = StatusRow().apply { applyKey("Queued", "—") }

    private val usageRow = StatusRow().apply { applyKey("Usage", "—") }

    private val verificationRow = StatusRow().apply { applyKey("Verification", "—") }

    private val filesRow = StatusRow().apply { applyKey("Files changed", "—") }

    private val indexRow = StatusRow().apply { applyKey("Index coverage", "—") }

    private val trustRow = StatusRow().apply { applyKey("Trust", "—") }

    private val rows = listOf(
        serviceRow, readinessRow, streamRow, stateRow, modelRow, toolRow,
        queuedRow, usageRow, verificationRow, filesRow, indexRow, trustRow
    )

    private val restartButton = secondaryButton("Restart service")

    private val reconnectButton = secondaryButton("Reconnect stream")

    private val refreshButton = secondaryButton("Refresh diagnostics")

    private val waitButton = secondaryButton("Wait")

    private val refreshT27Button = secondaryButton("Refresh")

    private val reconnectPausedButton = secondaryButton("Reconnect")

    private val refreshSnapshotButton = secondaryButton("Refresh from snapshot")

    private val recoveryHeadlineLabel = WrappedLabel("")

    private val recoveryBodyLabel = wrappedMutedLabel("")

    private val recoveryDetailLabel = wrappedMutedLabel("")

    private val recoveryNoneRow: JPanel

    private val recoveryReconnectingRow: JPanel

    private val recoveryPausedRow: JPanel

    private val setupHeadline = WrappedLabel(SANDBOX_SETUP_HEADLINE)

    private val setupCopy = wrappedMutedLabel(SANDBOX_SETUP_COPY)

    private val setupGuidance = wrappedMutedLabel(
        "Run this command in a terminal to record your explicit sandbox shell choice. " +
            "This panel never changes the sandbox: the daemon keeps its fail-closed default " +
            "until the choice is recorded by the CLI."
    )

    private val setupCommand = compactArea(1, monospace = true).apply {
        text = SANDBOX_GUIDANCE_COMMAND
        toolTipText = "Select and copy the guidance command"
        accessibleContext.accessibleName = "Sandbox guidance command: $SANDBOX_GUIDANCE_COMMAND"
    }

    private val setupButton = secondaryButton(SANDBOX_SETUP_ACTION)

    private var listener: Listener? = null

    private var available = true

    private var recoveryState = StreamRecoveryState.NONE

    private var recoveryDetail: String? = null

    init {
        val statusBody = pageColumn(gap = 6, padding = 0)
        for ((index, row) in rows.withIndex()) {
            if (index > 0) statusBody.add(vSpace(6))
            statusBody.add(row)
        }
        restartButton.addActionListener { listener?.onRestart() }
        reconnectButton.addActionListener { listener?.onReconnect() }
        refreshButton.addActionListener { listener?.onRefresh() }
        waitButton.addActionListener { listener?.onWaitForReconnect() }
        refreshT27Button.addActionListener { listener?.onRefresh() }
        refreshT27Button.toolTipText = "Re-read the durable state; the cursor is preserved"
        reconnectPausedButton.addActionListener { listener?.onReconnect() }
        reconnectPausedButton.toolTipText =
            "Reconnect from the last good position (after a service upgrade)"
        refreshSnapshotButton.addActionListener { listener?.onRefreshFromSnapshot() }
        refreshSnapshotButton.toolTipText =
            "Re-read the durable state; the blocked position is not skipped"
        recoveryHeadlineLabel.font = sectionTitleFont()
        recoveryHeadlineLabel.foreground = textForeground()
        recoveryNoneRow = actionRow(reconnectButton, restartButton, refreshButton)
        recoveryReconnectingRow = actionRow(waitButton, refreshT27Button)
        recoveryPausedRow = actionRow(reconnectPausedButton, refreshSnapshotButton)

        val connectionBody = pageColumn(gap = Spacing.XS, padding = 0)
        connectionBody.add(recoveryHeadlineLabel)
        connectionBody.add(recoveryBodyLabel)
        connectionBody.add(recoveryDetailLabel)
        connectionBody.add(vSpace(Spacing.XS))
        connectionBody.add(recoveryNoneRow)
        connectionBody.add(recoveryReconnectingRow)
        connectionBody.add(recoveryPausedRow)

        val setupBody = pageColumn(gap = Spacing.XS, padding = 0)
        setupHeadline.accessibleContext.accessibleName = SANDBOX_SETUP_HEADLINE
        setupBody.add(setupHeadline)
        setupBody.add(setupCopy)
        setupBody.add(vSpace(Spacing.XS))
        setupBody.add(setupCommand)
        setupGuidance.isVisible = false
        setupBody.add(setupGuidance)
        setupButton.addActionListener { revealSetupGuidance() }
        setupBody.add(actionRow(setupButton))

        val page = pageColumn()
        page.add(card("Session and service", statusBody))
        page.add(vSpace(Spacing.M))
        page.add(card("Recovery", connectionBody))
        page.add(vSpace(Spacing.M))
        page.add(card("Setup", setupBody))
        add(panelHeader(WrappedLabel("diagnostics")), BorderLayout.NORTH)
        add(pageScroll(page), BorderLayout.CENTER)
        applyRecoveryState()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun setDaemon(text: String) {
        val prefix = "daemon: "
        if (text.startsWith(prefix)) {
            serviceRow.applyKey("Service", text.substring(prefix.length))
        } else {
            serviceRow.apply(text)
        }
    }

    /** The served service state (`starting`/`running`/`failed`/`stopped`). */
    fun setService(state: String, detail: String?) {
        serviceRow.applyKey(
            "Service",
            state + (if (detail.isNullOrBlank()) "" else " · " + detail)
        )
    }

    /**
     * The readiness of the service start: `ready`, `waiting` or `timeout`
     * (with the typed reason). Anything never observed stays `—`.
     */
    fun setReadiness(state: String, detail: String?) {
        readinessRow.applyKey(
            "Readiness",
            state + (if (detail.isNullOrBlank()) "" else " · " + detail)
        )
    }

    fun setStream(text: String) {
        val prefix = "stream: "
        val livePrefix = "live updates: "
        when {
            text.startsWith(prefix) -> streamRow.applyKey("Live updates", text.substring(prefix.length))
            text.startsWith(livePrefix) ->
                streamRow.applyKey("Live updates", text.substring(livePrefix.length))
            else -> streamRow.apply(text)
        }
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
        val prefix = "index: "
        if (text.startsWith(prefix)) {
            indexRow.applyKey("Index coverage", text.substring(prefix.length))
        } else {
            indexRow.apply(text)
        }
    }

    fun setIndexCoverage(value: String) {
        indexRow.applyKey("Index coverage", value)
    }

    /**
     * The daemon binary trust state (D14 exact wording) plus the resolved
     * executable path; a never-resolved binary stays `—`.
     */
    fun setTrust(label: String, resolvedPath: String?) {
        val value = if (resolvedPath.isNullOrEmpty()) label else label + "\n" + resolvedPath
        trustRow.applyKey("Trust", value)
    }

    /** The connection readout: daemon description, live-update state, cursor. */
    fun setConnection(daemon: String, stream: String, cursor: Long, sessionId: String?) {
        setDaemon("daemon: $daemon")
        streamRow.applyKey(
            "Live updates",
            "$stream at cursor $cursor" +
                (if (sessionId == null) "" else " for this conversation")
        )
    }

    /** The typed recovery state (T27/T28) driven by the stream callbacks. */
    fun setStreamRecovery(state: StreamRecoveryState, detail: String? = null) {
        recoveryState = state
        recoveryDetail = detail
        applyRecoveryState()
    }

    fun streamRecoveryState(): StreamRecoveryState = recoveryState

    /** The verbatim headline of the visible recovery state. */
    fun recoveryHeadline(): String = recoveryHeadlineLabel.fullText

    /** The verbatim supporting copy of the visible recovery state. */
    fun recoveryBody(): String = recoveryBodyLabel.fullText

    /** The visible action labels of the recovery state (never a skip). */
    fun recoveryActions(): List<String> {
        val row = when (recoveryState) {
            StreamRecoveryState.NONE -> recoveryNoneRow
            StreamRecoveryState.RECONNECTING -> recoveryReconnectingRow
            StreamRecoveryState.PAUSED -> recoveryPausedRow
        }
        return row.components.filterIsInstance<JButton>().map { it.text }
    }

    fun setUnavailable(detailText: String) {
        available = false
        serviceRow.applyKey("Service", "unavailable")
        streamRow.applyKey("Live updates", "unavailable ($detailText)")
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

    fun restartEnabled(): Boolean = restartButton.isEnabled && restartButton.isVisible

    fun reconnectEnabled(): Boolean = reconnectButton.isEnabled && reconnectButton.isVisible

    fun available(): Boolean = available

    fun streamText(): String = streamRow.value.fullText

    fun daemonText(): String = serviceRow.value.fullText

    fun readinessText(): String = readinessRow.value.fullText

    fun trustText(): String = trustRow.value.fullText

    fun indexText(): String = indexRow.value.fullText

    /** Reveals and focuses the sandbox setup guidance (T22/T32 action). */
    fun revealSetupGuidance() {
        setupGuidance.isVisible = true
        setupCommand.requestFocusInWindow()
        setupCommand.selectAll()
        revalidate()
        repaint()
    }

    fun setupGuidanceVisibleForTest(): Boolean = setupGuidance.isVisible

    fun setupActionForTest(): JButton = setupButton

    fun sandboxCommandTextForTest(): String = setupCommand.text

    private fun applyRecoveryState() {
        when (recoveryState) {
            StreamRecoveryState.NONE -> {
                recoveryHeadlineLabel.isVisible = false
                recoveryBodyLabel.fullText =
                    "Reconnect resumes the live-update stream from its exact cursor; " +
                        "restart adopts a restarted service without duplicating journal frames."
                recoveryDetailLabel.isVisible = false
                recoveryNoneRow.isVisible = true
                recoveryReconnectingRow.isVisible = false
                recoveryPausedRow.isVisible = false
            }
            StreamRecoveryState.RECONNECTING -> {
                recoveryHeadlineLabel.fullText = RECONNECTING_HEADLINE
                recoveryHeadlineLabel.isVisible = true
                recoveryBodyLabel.fullText = RECONNECTING_COPY
                recoveryDetailLabel.isVisible = false
                recoveryNoneRow.isVisible = false
                recoveryReconnectingRow.isVisible = true
                recoveryPausedRow.isVisible = false
            }
            StreamRecoveryState.PAUSED -> {
                recoveryHeadlineLabel.fullText = STREAM_PAUSED_HEADLINE
                recoveryHeadlineLabel.isVisible = true
                recoveryBodyLabel.fullText = STREAM_PAUSED_COPY
                val detail = recoveryDetail
                if (detail.isNullOrBlank()) {
                    recoveryDetailLabel.isVisible = false
                } else {
                    recoveryDetailLabel.fullText = detail
                    recoveryDetailLabel.isVisible = true
                }
                recoveryNoneRow.isVisible = false
                recoveryReconnectingRow.isVisible = false
                recoveryPausedRow.isVisible = true
            }
        }
        revalidate()
        repaint()
    }
}
