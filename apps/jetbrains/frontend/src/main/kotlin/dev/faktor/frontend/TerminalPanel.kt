// Session-owned terminal surface over the native ACP/PTY routes:
// `GET /native/terminals?session=`, `GET /native/session/{id}/terminal/events`,
// `POST /native/session/{id}/terminal` and the output snapshot
// `GET /native/session/{id}/terminals/{terminalId}/output`. Rows carry their durable ownership
// (session/task/agent/operation/spawned ms); unowned legacy rows are shown
// only as the page's counted `note`, never mixed into the session list.
//
// Spawn composition is a FAITHFUL argv editor: one program field plus one
// argument per line (spaces inside a line are part of that argument, never a
// splitter), and an explicitly labeled shell-command mode for real shell
// input. Event pages and the output snapshot carry real bounded paging
// controls instead of an inert "(more available)" hint.
// Pure presentation: every action is delivered to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.NativeTerminal
import dev.faktor.shared.NativeTerminalEvent
import dev.faktor.shared.NativeTerminalEventPage
import dev.faktor.shared.NativeTerminalOutput
import dev.faktor.shared.NativeTerminalPage
import java.awt.BorderLayout
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.ListSelectionModel

class TerminalPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onRefresh()

        fun onSpawn(command: String, args: List<String>, cwd: String?)

        fun onOutput(ptyId: String)

        /** Loads the next event page strictly after [after]; default no-op. */
        fun onLoadEvents(after: Long) {}
    }

    private val terminalsModel = DefaultListModel<NativeTerminal>()

    private val terminalsList: JList<NativeTerminal> = TerminalList(terminalsModel)

    private val eventsArea = compactArea(4, monospace = true)

    private val outputArea = compactArea(6, monospace = true)

    private val header = WrappedLabel("session terminals: -")

    private val commandField = JTextField("bash", 12)

    /** One argument per line: spaces inside a line are one argv element. */
    private val argsArea = JTextArea(4, 18).apply {
        lineWrap = false
        font = monospacePanelFont()
    }

    private val shellModeCheck = JCheckBox("shell command mode")

    private val shellArea = JTextArea(3, 18).apply {
        lineWrap = true
        wrapStyleWord = true
        font = monospacePanelFont()
        isEnabled = false
    }

    private val cwdField = JTextField("", 10)

    private val spawnButton = primaryButton("Spawn")

    private val refreshButton = secondaryButton("Refresh")

    private val outputButton = secondaryButton("Show output")

    private val moreEventsButton = secondaryButton("Load more events")

    private val moreOutputButton = secondaryButton("Show more output")

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    private var eventsCursor: Long? = null

    private var eventsHasMore = false

    private var lastOutput: NativeTerminalOutput? = null

    private var outputDisplayCap = OUTPUT_INITIAL_CHARS

    init {
        terminalsList.selectionMode = ListSelectionModel.SINGLE_SELECTION
        terminalsList.visibleRowCount = 4
        RowRhythm.install(terminalsList)
        terminalsList.cellRenderer = TerminalCellRenderer()
        terminalsList.addListSelectionListener { updateButtons() }
        spawnButton.addActionListener { submitSpawn() }
        refreshButton.addActionListener { listener?.onRefresh() }
        outputButton.addActionListener {
            val terminal = terminalsList.selectedValue
            if (terminal != null) listener?.onOutput(terminal.id)
        }
        moreEventsButton.addActionListener { loadMoreEvents() }
        moreOutputButton.addActionListener { showMoreOutput() }
        shellModeCheck.addActionListener { updateComposerMode() }

        // One compact two-column grid: muted labels in the west column,
        // fields filling east, on the shared 4/8px spacing scale.
        val spawnForm = FormGrid()
            .row("Program", commandField)
            .row("Working dir (optional)", cwdField)
            .build()
        cwdField.toolTipText = "Working directory the terminal starts in (daemon-relative when empty)"
        val argsHint = wrappedMutedLabel(
            "Arguments — one per line; spaces inside a line stay one argument"
        )
        val argsScroll = insetScroll(argsArea)
        val shellRow = JPanel(BorderLayout(Spacing.S, 0))
        shellRow.isOpaque = false
        shellRow.add(shellModeCheck, BorderLayout.WEST)
        shellRow.add(insetScroll(shellArea), BorderLayout.CENTER)
        val spawnBody = pageColumn(gap = Spacing.S, padding = 0)
        spawnBody.add(spawnForm)
        spawnBody.add(argsHint)
        spawnBody.add(vSpace(Spacing.XS))
        spawnBody.add(argsScroll)
        spawnBody.add(vSpace(Spacing.XS))
        spawnBody.add(shellRow)
        spawnBody.add(vSpace(Spacing.XS))
        spawnBody.add(actionRow(spawnButton, refreshButton))

        val terminalsScroll = insetScroll(terminalsList)
        val terminalsBody = JPanel(BorderLayout(0, Spacing.S))
        terminalsBody.isOpaque = false
        terminalsBody.add(terminalsScroll, BorderLayout.CENTER)
        terminalsBody.add(actionRow(outputButton), BorderLayout.SOUTH)

        val eventsBody = JPanel(BorderLayout(0, Spacing.S))
        eventsBody.isOpaque = false
        eventsBody.add(insetScroll(eventsArea), BorderLayout.CENTER)
        eventsBody.add(actionRow(moreEventsButton), BorderLayout.SOUTH)

        val outputBody = JPanel(BorderLayout(0, Spacing.S))
        outputBody.isOpaque = false
        outputBody.add(insetScroll(outputArea), BorderLayout.CENTER)
        outputBody.add(actionRow(moreOutputButton), BorderLayout.SOUTH)

        // BoxLayout Y keeps every card's own preferred height; the whole
        // body scrolls in a short tool window instead of squashing the
        // structured argv editor down to zero.
        val body = pageColumn()
        body.add(card("Session terminals", terminalsBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Spawn terminal", spawnBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Lifetime events", eventsBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Output snapshot", outputBody))
        val bodyScroll = JScrollPane(body)
        bodyScroll.verticalScrollBar.unitIncrement = 16
        add(panelHeader(header), BorderLayout.NORTH)
        add(bodyScroll, BorderLayout.CENTER)
        updateComposerMode()
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
            "No session-owned terminals. Spawn one below to run commands on it."
        } else {
            "unowned daemon-level rows: ${page.unowned}\n${page.note}"
        }
        updateButtons()
    }

    /** Replaces the event view with one page and arms the Load-more control. */
    fun setEvents(page: NativeTerminalEventPage) {
        eventsCursor = page.nextCursor
        eventsHasMore = page.hasMore
        if (page.events.isEmpty()) {
            eventsArea.text = "No lifetime events yet for session ${page.sessionId}."
        } else {
            eventsArea.text = renderEvents(page.events)
        }
        updateButtons()
    }

    /** Appends the next event page (Load more events). */
    fun appendEvents(page: NativeTerminalEventPage) {
        eventsCursor = page.nextCursor
        eventsHasMore = page.hasMore
        val chunk = renderEvents(page.events)
        if (chunk.isNotEmpty()) {
            val existing = eventsArea.text
            eventsArea.text = if (existing.isBlank()) {
                chunk
            } else {
                existing.trimEnd('\n') + "\n" + chunk
            }
        }
        updateButtons()
    }

    private fun renderEvents(events: List<NativeTerminalEvent>): String {
        val text = StringBuilder()
        for (event in events) {
            text.append('#').append(event.id).append(' ').append(event.type)
            text.append(" pty=").append(event.ptyId).append(" pid=").append(event.pid)
            text.append(" at=").append(event.tsMs).append("ms")
            text.append('\n')
        }
        return text.toString()
    }

    /** Loads the next event page exactly as the Load more events control. */
    fun loadMoreEvents() {
        if (!moreEventsButton.isEnabled) return
        val cursor = eventsCursor ?: return
        listener?.onLoadEvents(cursor)
    }

    /**
     * One output snapshot. The display is bounded; the "Show more output"
     * control grows the bounded window explicitly (never a silent loss) and
     * the header names exactly how much of the snapshot is shown.
     */
    fun setOutput(output: NativeTerminalOutput) {
        lastOutput = output
        outputDisplayCap = OUTPUT_INITIAL_CHARS
        renderOutput()
    }

    /** Grows the bounded display window by one bounded step. */
    fun showMoreOutput() {
        if (outputDisplayCap >= OUTPUT_MAX_CHARS) return
        outputDisplayCap = Math.min(OUTPUT_MAX_CHARS, outputDisplayCap * 4)
        renderOutput()
    }

    private fun renderOutput() {
        val output = lastOutput
        if (output == null) {
            outputArea.text = "Select a terminal and load its output snapshot."
            moreOutputButton.isEnabled = false
            return
        }
        val total = output.output.length
        val shown = Math.min(total, outputDisplayCap)
        val truncated = shown < total
        val head = "pty ${output.ptyId} alive=${output.alive}" +
            if (truncated) {
                " (showing $shown of $total chars)" +
                    if (shown >= OUTPUT_MAX_CHARS) "; display cap reached" else "; truncated"
            } else {
                ""
            }
        outputArea.text = head + "\n" + output.output.substring(0, shown)
        moreOutputButton.isEnabled = truncated && shown < OUTPUT_MAX_CHARS
    }

    /** One honest note in the events area (e.g. a typed refusal). */
    fun setEventsNote(text: String) {
        eventsCursor = null
        eventsHasMore = false
        eventsArea.text = text
        updateButtons()
    }

    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        terminalsModel.clear()
        eventsCursor = null
        eventsHasMore = false
        lastOutput = null
        eventsArea.text = detailText
        outputArea.text = detailText
        updateButtons()
    }

    fun terminalCount(): Int = terminalsModel.size()

    /** One bounded row label (the renderer and the test helper share it). */
    private fun rowLabel(terminal: NativeTerminal): String {
        val owner = StringBuilder()
        if (terminal.sessionId != null) owner.append(" session=").append(terminal.sessionId)
        if (terminal.taskId != null) owner.append(" task=").append(terminal.taskId)
        if (terminal.agentId != null) owner.append(" agent=").append(terminal.agentId)
        if (terminal.operationId != null) owner.append(" op=").append(terminal.operationId)
        if (terminal.spawnedMs != null) owner.append(" spawned=").append(terminal.spawnedMs)
        return "pty ${terminal.id} pid=${terminal.pid} " +
            (if (terminal.alive) "alive" else "exited") + owner
    }

    fun terminalLabel(index: Int): String = rowLabel(terminalsModel.getElementAt(index))

    /**
     * The list renders the bounded row label, never the raw DTO toString
     * (which used to clip at "NativeTerminal(id=5, pid=123, alive=true,").
     * A narrowed row keeps the full label as its tooltip.
     */
    private inner class TerminalCellRenderer : javax.swing.DefaultListCellRenderer() {
        override fun getListCellRendererComponent(
            list: JList<*>?,
            value: Any?,
            index: Int,
            selected: Boolean,
            focus: Boolean
        ): java.awt.Component {
            super.getListCellRendererComponent(list, value, index, selected, focus)
            if (!selected) {
                RowRhythm.hoverBackground(list, index)?.let { background = it }
            }
            val terminal = value as? NativeTerminal ?: return this
            text = rowLabel(terminal)
            return this
        }
    }

    /** Per-cell tooltips: the full row even when the cell is narrowed. */
    private inner class TerminalList(model: DefaultListModel<NativeTerminal>) :
        JList<NativeTerminal>(model) {
        override fun getToolTipText(event: java.awt.event.MouseEvent?): String? {
            val point = event?.point ?: return null
            val index = locationToIndex(point)
            val bounds = getCellBounds(index, index)
            if (index < 0 || bounds == null || !bounds.contains(point)) return null
            return rowLabel(model.getElementAt(index))
        }
    }

    fun headerText(): String = header.fullText

    fun eventsText(): String = eventsArea.text

    fun outputText(): String = outputArea.text

    fun available(): Boolean = available

    fun spawnEnabled(): Boolean = available

    fun loadMoreEventsEnabled(): Boolean = moreEventsButton.isEnabled

    fun showMoreOutputEnabled(): Boolean = moreOutputButton.isEnabled

    fun shellModeSelected(): Boolean = shellModeCheck.isSelected

    fun select(index: Int) {
        terminalsList.selectedIndex = index
    }

    fun selectedTerminalId(): String? = terminalsList.selectedValue?.id

    fun setComposerFields(command: String, args: String, cwd: String) {
        commandField.text = command
        argsArea.text = args
        cwdField.text = cwd
    }

    fun setShellMode(enabled: Boolean) {
        shellModeCheck.isSelected = enabled
        updateComposerMode()
    }

    fun setShellCommand(command: String) {
        shellArea.text = command
    }

    /**
     * One line = one argument, verbatim: interior spaces survive, and an
     * empty line is not an argument. This is NOT whitespace splitting.
     */
    internal fun argumentLines(text: String): List<String> =
        text.split('\n')
            .map { it.trimEnd('\r') }
            .filter { it.isNotEmpty() }

    /** The platform shell invocation of one explicit shell command. */
    internal fun shellArgv(command: String): List<String> =
        if (System.getProperty("os.name", "").lowercase().contains("win")) {
            listOf("cmd.exe", "/c", command)
        } else {
            listOf("/bin/sh", "-lc", command)
        }

    /** Submits the composer exactly like the Spawn button. */
    fun submitSpawn() {
        if (!available) return
        val cwd = cwdField.text.trim().ifEmpty { null }
        if (shellModeCheck.isSelected) {
            val command = shellArea.text
            if (command.isBlank()) {
                eventsArea.text = "spawn refused: no shell command entered"
                return
            }
            val argv = shellArgv(command)
            listener?.onSpawn(argv[0], argv.drop(1), cwd)
            return
        }
        val command = commandField.text.trim()
        if (command.isEmpty()) {
            eventsArea.text = "spawn refused: no command entered"
            return
        }
        listener?.onSpawn(command, argumentLines(argsArea.text), cwd)
    }

    private fun updateComposerMode() {
        val shell = shellModeCheck.isSelected
        commandField.isEnabled = !shell
        argsArea.isEnabled = !shell
        shellArea.isEnabled = shell
    }

    private fun updateButtons() {
        val selected = terminalsList.selectedValue != null
        spawnButton.isEnabled = available
        outputButton.isEnabled = available && selected
        refreshButton.isEnabled = true
        moreEventsButton.isEnabled = available && eventsHasMore && eventsCursor != null
        moreOutputButton.isEnabled = available && lastOutput?.let {
            it.output.length > outputDisplayCap && outputDisplayCap < OUTPUT_MAX_CHARS
        } == true
        header.text = if (!available) {
            "session terminals: unavailable ($reason)"
        } else {
            "session terminals: ${terminalsModel.size()}"
        }
    }

    companion object {
        /** The initial bounded output window (chars). */
        const val OUTPUT_INITIAL_CHARS = 8000

        /** The hard bounded display window; beyond this the header is honest. */
        const val OUTPUT_MAX_CHARS = 64 * 1024
    }
}
