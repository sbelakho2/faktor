// Session history + daemon restart/reconnect surface (upstream
// `session/history` and the restart/connection controls): the durable
// session list (`GET /native/sessions`), selection that reopens a session, and
// explicit daemon restart / SSE reconnect controls. The reconnect readout
// shows the exact cursor the stream will resume from, so a restart can
// neither duplicate nor skip journal frames.
package dev.faktor.frontend

import dev.faktor.shared.NativeSessionSummary
import java.awt.BorderLayout
import java.awt.FlowLayout
import java.awt.GridLayout
import javax.swing.DefaultListModel
import javax.swing.JButton
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

    private val openButton = JButton("Open session")

    private val restartButton = JButton("Restart daemon")

    private val reconnectButton = JButton("Reconnect stream")

    private val refreshButton = JButton("Refresh history")

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
        val actions = JPanel(GridLayout(0, 1, 4, 4))
        actions.add(openButton)
        actions.add(refreshButton)
        actions.add(reconnectButton)
        actions.add(restartButton)
        val body = JPanel(GridLayout(0, 1, 0, 4))
        body.add(titledSection("sessions", JScrollPane(list)))
        body.add(titledSection("connection", actions))
        val south = JPanel(GridLayout(0, 1, 0, 2))
        south.add(daemonLabel)
        south.add(streamLabel)
        add(status, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        add(south, BorderLayout.SOUTH)
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

    fun label(index: Int): String {
        val session = model.getElementAt(index)
        val current = if (session.id == currentSessionId) " (current)" else ""
        return "${session.id} [${session.state}] ${session.provider}/${session.model} " +
            "price ${bound(session.title, 80)}$current"
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
            "history: ${model.size} session(s), current=${currentSessionId ?: "-"}"
        }
    }
}
