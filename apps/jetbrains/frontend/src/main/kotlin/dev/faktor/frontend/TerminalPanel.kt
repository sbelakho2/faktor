// Commands: the session's process surface over the native ACP/PTY routes
// (`GET /native/terminals?session=`, `GET /native/session/{id}/terminal/events`,
// `POST /native/session/{id}/terminal` and the output snapshot
// `GET /native/session/{id}/terminals/{terminalId}/output`). The primary rows
// read like a shell engine: `$ program args` with the working directory, then
// the process state and its live output. PTY ids, pids, owners and spawn
// lifetimes stay under the Advanced disclosure. Spawn composition is a
// FAITHFUL argv editor: one program field plus one argument per line (spaces
// inside a line are part of that argument, never a splitter), and an
// explicitly labeled shell-command mode for real shell input. Pure
// presentation: every action is delivered to a Listener.
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

    /** One locally observed spawn, so a command row can name its argv. */
    private data class SpawnSpec(val command: String, val args: List<String>, val cwd: String?) {
        fun shellLine(): String {
            val parts = ArrayList<String>()
            parts.add(command)
            for (arg in args) {
                parts.add(if (arg.any { it == ' ' }) "\"" + arg + "\"" else arg)
            }
            val line = parts.joinToString(" ")
            return if (cwd.isNullOrBlank()) line else "$line   (in $cwd)"
        }
    }

    private val terminalsModel = DefaultListModel<NativeTerminal>()

    private val terminalsList: JList<NativeTerminal> = TerminalList(terminalsModel)

    private val eventsArea = compactArea(4, monospace = true)

    private val outputArea = compactArea(6, monospace = true)

    private val advancedArea = compactArea(4, monospace = true)

    private val header = WrappedLabel("commands: -")

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

    private val spawnButton = primaryButton("Run")

    private val refreshButton = secondaryButton("Refresh")

    private val outputButton = secondaryButton("Show output")

    private val moreEventsButton = secondaryButton("Load more events")

    private val moreOutputButton = secondaryButton("Show more output")

    private val advancedToggle = secondaryButton("Show advanced")

    private val advancedBody = JPanel(BorderLayout())

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    private var eventsCursor: Long? = null

    private var eventsHasMore = false

    private var lastOutput: NativeTerminalOutput? = null

    private var outputDisplayCap = OUTPUT_INITIAL_CHARS

    /** Bounded local record of argv observed at spawn time (id to spec). */
    private val spawns = LinkedHashMap<String, SpawnSpec>()

    init {
        terminalsList.selectionMode = ListSelectionModel.SINGLE_SELECTION
        terminalsList.visibleRowCount = 4
        RowRhythm.install(terminalsList)
        terminalsList.cellRenderer = CommandCellRenderer()
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
        cwdField.toolTipText = "Working directory the command starts in (service-relative when empty)"
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

        val outputBody = JPanel(BorderLayout(0, Spacing.S))
        outputBody.isOpaque = false
        outputBody.add(insetScroll(outputArea), BorderLayout.CENTER)
        outputBody.add(actionRow(moreOutputButton), BorderLayout.SOUTH)

        // Advanced: raw process ownership, lifetime events and the PTY ids
        // the primary surface deliberately keeps out of the way.
        val eventsBody = JPanel(BorderLayout(0, Spacing.S))
        eventsBody.isOpaque = false
        eventsBody.add(insetScroll(eventsArea), BorderLayout.CENTER)
        eventsBody.add(actionRow(moreEventsButton), BorderLayout.SOUTH)
        val advancedColumn = pageColumn(gap = Spacing.S, padding = 0)
        advancedColumn.add(sectionHeader("Process ownership", muted = true))
        advancedColumn.add(insetScroll(advancedArea))
        advancedColumn.add(sectionHeader("Lifetime events", muted = true))
        advancedColumn.add(eventsBody)
        advancedToggle.addActionListener {
            advancedBody.isVisible = advancedToggle.isSelected
            advancedToggle.text = if (advancedToggle.isSelected) {
                "Hide advanced"
            } else {
                "Show advanced"
            }
            revalidate()
            repaint()
        }
        advancedBody.isOpaque = false
        advancedBody.add(advancedColumn, BorderLayout.CENTER)
        advancedBody.isVisible = false

        // BoxLayout Y keeps every card's own preferred height; the whole
        // body scrolls in a short tool window instead of squashing the
        // structured argv editor down to zero.
        val body = pageColumn()
        body.add(card("Commands", terminalsBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Live output", outputBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Run a command", spawnBody))
        body.add(vSpace(Spacing.M))
        body.add(advancedToggle)
        body.add(vSpace(Spacing.XS))
        body.add(advancedBody)
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
        advancedArea.text = if (page.terminals.isEmpty()) {
            "No session-owned processes."
        } else {
            val sb = StringBuilder()
            for (terminal in page.terminals) sb.append(rowLabel(terminal)).append('\n')
            sb.toString()
        }
        if (page.note.isNotEmpty()) {
            // The service-level note belongs with the lifetime readout so the
            // unowned count is visible without opening Advanced.
            eventsArea.text = "unowned daemon-level rows: ${page.unowned}\n${page.note}"
        }
        updateButtons()
    }

    /**
     * Records the argv of one successful local spawn so the command row can
     * render `$ program args`. Bounded: only the newest [MAX_SPAWN_SPECS]
     * specs are retained; pty ids are never reused by the service.
     */
    fun noteSpawned(ptyId: String, command: String, args: List<String>, cwd: String?) {
        spawns[ptyId] = SpawnSpec(command, args, cwd)
        while (spawns.size > MAX_SPAWN_SPECS) {
            val oldest = spawns.keys.firstOrNull() ?: break
            spawns.remove(oldest)
        }
        terminalsList.repaint()
    }

    /** Replaces the event view with one page and arms the Load-more control. */
    fun setEvents(page: NativeTerminalEventPage) {
        eventsCursor = page.nextCursor
        eventsHasMore = page.hasMore
        if (page.events.isEmpty()) {
            eventsArea.text = "No lifetime events yet for this conversation."
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
            outputArea.text = "Select a command and show its output snapshot."
            moreOutputButton.isEnabled = false
            return
        }
        val total = output.output.length
        val shown = Math.min(total, outputDisplayCap)
        val truncated = shown < total
        val head = "command output" +
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
        advancedArea.text = detailText
        updateButtons()
    }

    fun terminalCount(): Int = terminalsModel.size()

    /** One bounded ADVANCED row label: pty/pid/owners/lifetime. */
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

    /** The advanced pty row (smoke observability; the UI shows command rows). */
    fun terminalLabel(index: Int): String = rowLabel(terminalsModel.getElementAt(index))

    /**
     * The PRIMARY command row: `$ program args (in cwd) · state` when the
     * argv was observed locally, otherwise an honest placeholder — never a
     * fabricated command line.
     */
    internal fun commandLabel(terminal: NativeTerminal): String {
        val state = if (terminal.alive) "running" else "exited"
        val spec = spawns[terminal.id]
        return if (spec == null) {
            "Command (arguments not reported) · $state"
        } else {
            "$ " + spec.shellLine() + " · " + state
        }
    }

    /** The rendered human command rows (smoke observable). */
    fun commandsText(): String {
        val sb = StringBuilder()
        for (i in 0 until terminalsModel.size()) {
            sb.append(commandLabel(terminalsModel.getElementAt(i))).append('\n')
        }
        return sb.toString()
    }

    /**
     * The list renders the primary command row; the raw pty row stays one
     * disclosure away (advanced area + tooltip).
     */
    private inner class CommandCellRenderer : javax.swing.DefaultListCellRenderer() {
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
            text = commandLabel(terminal)
            toolTipText = rowLabel(terminal)
            return this
        }
    }

    /** Per-cell tooltips: the full advanced row even when the cell is narrowed. */
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

    fun advancedText(): String = advancedArea.text

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

    /** Submits the composer exactly like the Run button. */
    fun submitSpawn() {
        if (!available) return
        val cwd = cwdField.text.trim().ifEmpty { null }
        if (shellModeCheck.isSelected) {
            val command = shellArea.text
            if (command.isBlank()) {
                eventsArea.text = "run refused: no shell command entered"
                return
            }
            val argv = shellArgv(command)
            listener?.onSpawn(argv[0], argv.drop(1), cwd)
            return
        }
        val command = commandField.text.trim()
        if (command.isEmpty()) {
            eventsArea.text = "run refused: no command entered"
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
            "commands: unavailable ($reason)"
        } else {
            "commands: ${terminalsModel.size()}"
        }
    }

    companion object {
        /** The initial bounded output window (chars). */
        const val OUTPUT_INITIAL_CHARS = 8000

        /** The hard bounded display window; beyond this the header is honest. */
        const val OUTPUT_MAX_CHARS = 64 * 1024

        /** The bounded local spawn-spec window. */
        const val MAX_SPAWN_SPECS = 64
    }
}
