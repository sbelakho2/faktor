// Session history + daemon restart/reconnect surface (upstream
// `session/history` and the restart/connection controls): the durable
// session list (`GET /native/sessions`), selection that reopens a session, and
// explicit daemon restart / SSE reconnect controls. The reconnect readout
// shows the exact cursor the stream will resume from, so a restart can
// neither duplicate nor skip journal frames.
package dev.faktor.frontend

import dev.faktor.shared.NativeSessionSummary
import java.awt.BorderLayout
import javax.swing.BorderFactory
import javax.swing.DefaultListModel
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.ListSelectionModel

class HistoryPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onOpenSession(sessionId: String)

        fun onRestart()

        fun onReconnect()

        fun onRefresh()
    }

    private val model = DefaultListModel<NativeSessionSummary>()

    private val list = JList(model)

    private val status = JLabel("history: -")

    private val streamLabel = JLabel("stream: off cursor=0")

    private val daemonLabel = JLabel("daemon: stopped")

    private val openButton = primaryButton("Open session")

    private val restartButton = secondaryButton("Restart daemon")

    private val reconnectButton = secondaryButton("Reconnect stream")

    private val refreshButton = secondaryButton("Refresh history")

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    private var currentSessionId: String? = null

    init {
        list.selectionMode = ListSelectionModel.SINGLE_SELECTION
        list.addListSelectionListener { updateButtons() }
        openButton.addActionListener {
            val session = list.selectedValue
            if (session != null) listener?.onOpenSession(session.id)
        }
        restartButton.addActionListener { listener?.onRestart() }
        reconnectButton.addActionListener { listener?.onReconnect() }
        refreshButton.addActionListener { listener?.onRefresh() }
        val sessionsBody = JPanel(BorderLayout(0, Spacing.S))
        sessionsBody.isOpaque = false
        sessionsBody.add(JScrollPane(list), BorderLayout.CENTER)
        sessionsBody.add(actionRow(openButton, refreshButton), BorderLayout.SOUTH)
        daemonLabel.font = uiPanelFont()
        streamLabel.font = uiPanelFont()
        daemonLabel.foreground = mutedForeground()
        streamLabel.foreground = mutedForeground()
        val connectionBody = pageColumn(gap = Spacing.XS, padding = 0)
        connectionBody.add(daemonLabel)
        connectionBody.add(streamLabel)
        connectionBody.add(vSpace(Spacing.XS))
        connectionBody.add(actionRow(reconnectButton, restartButton))
        val body = pageColumn()
        body.add(card("Sessions", sessionsBody))
        body.add(vSpace(Spacing.S))
        body.add(card("Connection", connectionBody))
        status.font = sectionTitleFont()
        val statusRow = JPanel(BorderLayout())
        statusRow.isOpaque = true
        statusRow.background = panelSurface()
        statusRow.border = BorderFactory.createEmptyBorder(
            Spacing.S, Spacing.M, 0, Spacing.M
        )
        statusRow.add(status, BorderLayout.CENTER)
        add(statusRow, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        updateButtons()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun update(sessions: List<NativeSessionSummary>, currentId: String?) {
        available = true
        reason = null
        currentSessionId = currentId
        val selectedId = list.selectedValue?.id
        model.clear()
        for (session in sessions) model.addElement(session)
        val preferred = selectedId ?: currentId
        if (preferred != null) {
            for (i in 0 until model.size()) {
                if (model.getElementAt(i).id == preferred) {
                    list.selectedIndex = i
                    break
                }
            }
        } else if (model.size() > 0) {
            list.selectedIndex = 0
        }
        updateButtons()
    }

    fun setConnection(daemon: String, stream: String, cursor: Long, sessionId: String?) {
        daemonLabel.text = "daemon: $daemon"
        streamLabel.text = "stream: $stream cursor=$cursor session=${sessionId ?: "-"}"
    }

    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        model.clear()
        status.text = "history: unavailable ($reason)"
        updateButtons()
    }

    fun count(): Int = model.size()

    fun available(): Boolean = available

    fun label(index: Int): String = sessionLabel(model.getElementAt(index))

    /** One bounded session row (the renderer and the test helper share it). */
    private fun sessionLabel(session: NativeSessionSummary): String {
        val current = if (session.id == currentSessionId) " (current)" else ""
        return "${session.id} [${session.state}] ${session.provider}/${session.model} " +
            "${bound(session.title, 80)}$current"
    }

    /**
     * The REAL list path must use the same bounded label as [label]: without
     * a renderer, DefaultListCellRenderer shows NativeSessionSummary
     * .toString() raw (unbounded upstream title).
     */
    private inner class SessionCellRenderer : javax.swing.DefaultListCellRenderer() {
        override fun getListCellRendererComponent(
            list: JList<*>?,
            value: Any?,
            index: Int,
            selected: Boolean,
            focus: Boolean
        ): java.awt.Component {
            super.getListCellRendererComponent(list, value, index, selected, focus)
            val session = value as? NativeSessionSummary ?: return this
            text = sessionLabel(session)
            return this
        }
    }

    init {
        list.cellRenderer = SessionCellRenderer()
    }

    fun select(index: Int) {
        list.selectedIndex = index
    }

    fun selectedId(): String? = list.selectedValue?.id

    fun openEnabled(): Boolean = openButton.isEnabled

    fun restartEnabled(): Boolean = restartButton.isEnabled

    fun reconnectEnabled(): Boolean = reconnectButton.isEnabled

    fun statusText(): String = status.text

    fun streamText(): String = streamLabel.text

    /** Programmatic open, exactly like a click on Open session. */
    fun submitOpen() {
        val session = list.selectedValue
        if (session != null) listener?.onOpenSession(session.id)
    }

    private fun updateButtons() {
        openButton.isEnabled = available && list.selectedValue != null
        restartButton.isEnabled = true
        reconnectButton.isEnabled = available && currentSessionId != null
        status.text = if (!available) {
            "history: unavailable ($reason)"
        } else {
            "history: ${plural(model.size, "session")}, current=${currentSessionId ?: "-"}"
        }
    }
}
