// Session-owned terminal surface over the native ACP/PTY routes:
// `GET /native/terminals?session=`, `GET /native/session/{id}/terminal/events`,
// `POST /native/session/{id}/terminal` and the output snapshot
// `GET /pty/{ptyId}/output`. Rows carry their durable ownership
// (session/task/agent/operation/spawned ms); unowned legacy rows are shown
// only as the page's counted `note`, never mixed into the session list.
// Pure presentation: every action is delivered to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.NativeTerminal
import dev.faktor.shared.NativeTerminalEventPage
import dev.faktor.shared.NativeTerminalOutput
import dev.faktor.shared.NativeTerminalPage
import java.awt.BorderLayout
import java.awt.FlowLayout
import java.awt.GridLayout
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextField
import javax.swing.ListSelectionModel

class TerminalPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onRefresh()

        fun onSpawn(command: String, args: List<String>, cwd: String?)

        fun onOutput(ptyId: String)
    }

    private val terminalsModel = DefaultListModel<NativeTerminal>()

    private val terminalsList = JList(terminalsModel)

    private val eventsArea = compactArea(4)

    private val outputArea = compactArea(6)

    private val header = JLabel("session terminals: -")

    private val commandField = JTextField("bash", 10)

    private val argsField = JTextField("", 10)

    private val cwdField = JTextField("", 10)

    private val spawnButton = JButton("Spawn")

    private val refreshButton = JButton("Refresh")

    private val outputButton = JButton("Show output")

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    init {
        terminalsList.selectionMode = ListSelectionModel.SINGLE_SELECTION
        terminalsList.addListSelectionListener { updateButtons() }
        spawnButton.addActionListener { submitSpawn() }
        refreshButton.addActionListener { listener?.onRefresh() }
        outputButton.addActionListener {
            val terminal = terminalsList.selectedValue
            if (terminal != null) listener?.onOutput(terminal.id)
        }
        val form = JPanel(GridLayout(2, 3, 4, 4))
        form.add(JLabel("command"))
        form.add(JLabel("args (space separated)"))
        form.add(JLabel("cwd (optional)"))
        form.add(commandField)
        form.add(argsField)
        form.add(cwdField)
        val actions = JPanel(FlowLayout(FlowLayout.LEFT, 4, 0))
        actions.add(spawnButton)
        actions.add(refreshButton)
        actions.add(outputButton)
        outputArea.isEditable = false
        eventsArea.isEditable = false
        val body = JPanel(GridLayout(0, 1, 0, 4))
        body.add(titledSection("session terminals", JScrollPane(terminalsList)))
        body.add(titledSection("spawn a session-owned terminal", form))
        body.add(titledSection("actions", actions))
        body.add(titledSection("lifetime events", JScrollPane(eventsArea)))
        body.add(titledSection("output snapshot", JScrollPane(outputArea)))
        add(header, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        updateButtons()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    fun update(page: NativeTerminalPage) {
        available = true
        reason = null
        val selectedId = terminalsList.selectedValue?.id
        terminalsModel.clear()
        for (terminal in page.terminals) terminalsModel.addElement(terminal)
        if (selectedId != null) {
            for (i in 0 until terminalsModel.size()) {
                if (terminalsModel.getElementAt(i).id == selectedId) {
                    terminalsList.selectedIndex = i
                    break
                }
            }
        } else if (terminalsModel.size() > 0) {
            terminalsList.selectedIndex = 0
        }
        eventsArea.text = if (page.note.isEmpty()) {
            "no session-owned terminals"
        } else {
            "unowned daemon-level rows: ${page.unowned}\n${page.note}"
        }
        updateButtons()
    }

    fun setEvents(page: NativeTerminalEventPage) {
        if (page.events.isEmpty()) {
            eventsArea.text = "no lifetime events (session ${page.sessionId})"
            return
        }
        val text = StringBuilder()
        for (event in page.events) {
            text.append('#').append(event.id).append(' ').append(event.type)
            text.append(" pty=").append(event.ptyId).append(" pid=").append(event.pid)
            text.append(" at=").append(event.tsMs).append("ms")
            if (page.hasMore) text.append(" (more available)")
            text.append('\n')
        }
        eventsArea.text = text.toString()
    }

    fun setOutput(output: NativeTerminalOutput) {
        outputArea.text = "pty ${output.ptyId} alive=${output.alive}\n" +
            bound(output.output, 8000)
    }

    /** One honest note in the events area (e.g. a typed refusal). */
    fun setEventsNote(text: String) {
        eventsArea.text = text
    }

    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        terminalsModel.clear()
        eventsArea.text = detailText
        outputArea.text = detailText
        updateButtons()
    }

    fun terminalCount(): Int = terminalsModel.size()

    fun terminalLabel(index: Int): String {
        val terminal = terminalsModel.getElementAt(index)
        val owner = StringBuilder()
        if (terminal.sessionId != null) owner.append(" session=").append(terminal.sessionId)
        if (terminal.taskId != null) owner.append(" task=").append(terminal.taskId)
        if (terminal.agentId != null) owner.append(" agent=").append(terminal.agentId)
        if (terminal.operationId != null) owner.append(" op=").append(terminal.operationId)
        if (terminal.spawnedMs != null) owner.append(" spawned=").append(terminal.spawnedMs)
        return "pty ${terminal.id} pid=${terminal.pid} " +
            (if (terminal.alive) "alive" else "exited") + owner
    }

    fun headerText(): String = header.text

    fun eventsText(): String = eventsArea.text

    fun outputText(): String = outputArea.text

    fun available(): Boolean = available

    fun spawnEnabled(): Boolean = available

    fun select(index: Int) {
        terminalsList.selectedIndex = index
    }

    fun selectedTerminalId(): String? = terminalsList.selectedValue?.id

    fun setComposerFields(command: String, args: String, cwd: String) {
        commandField.text = command
        argsField.text = args
        cwdField.text = cwd
    }

    /** Submits the composer exactly like the Spawn button. */
    fun submitSpawn() {
        if (!available) return
        val command = commandField.text.trim()
        if (command.isEmpty()) return
        val args = argsField.text.split(' ')
            .map { it.trim() }
            .filter { it.isNotEmpty() }
        val cwd = cwdField.text.trim().ifEmpty { null }
        listener?.onSpawn(command, args, cwd)
    }

    private fun updateButtons() {
        val selected = terminalsList.selectedValue != null
        spawnButton.isEnabled = available
        outputButton.isEnabled = available && selected
        refreshButton.isEnabled = true
        header.text = if (!available) {
            "session terminals: unavailable ($reason)"
        } else {
            "session terminals: ${terminalsModel.size()}"
        }
    }
}
