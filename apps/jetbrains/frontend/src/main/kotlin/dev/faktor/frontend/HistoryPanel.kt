// Prior-work history: the durable session listing (`GET /native/sessions`)
// rendered as human rows — title, time and outcome (✓ Completed /
// ⚠ Needs attention / ✗ Failed) — with a local search/filter and one Open
// action that reopens the selected conversation. Daemon state, stream
// cursors, reconnect and restart deliberately live on the Diagnostics view,
// not here: this surface answers "what did I do before?" and nothing else.
package dev.faktor.frontend

import dev.faktor.shared.NativeSessionSummary
import java.awt.BorderLayout
import java.time.Instant
import java.time.ZoneOffset
import java.time.format.DateTimeFormatter
import javax.swing.DefaultListModel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.ListSelectionModel

/** One row's outcome: the human label plus the semantic tone it renders in. */
internal data class HistoryOutcome(val label: String, val tone: SemanticState)

/**
 * Maps a durable session state onto the three-outcome vocabulary. The
 * mapping is deliberately conservative: a live/idle session is Open, never
 * "Completed"; anything unrecognized stays Unknown rather than being
 * promoted to success.
 */
internal fun historyOutcomeOf(state: String, current: Boolean): HistoryOutcome {
    val normalized = state.lowercase()
    return when {
        normalized.contains("fail") || normalized.contains("permanent") ->
            HistoryOutcome("✗ Failed", SemanticState.NEGATIVE)
        normalized.contains("cancel") ->
            HistoryOutcome("⚠ Needs attention", SemanticState.WARNING)
        normalized.contains("await") || normalized.contains("permission") ||
            normalized.contains("needs") || normalized.contains("input") ||
            normalized.contains("block") || normalized.contains("stall") ->
            HistoryOutcome("⚠ Needs attention", SemanticState.WARNING)
        normalized.contains("complete") || normalized.contains("ended") || normalized.contains("closed") ->
            HistoryOutcome("✓ Completed", SemanticState.POSITIVE)
        normalized.contains("ready") || normalized.contains("idle") ||
            normalized.contains("running") || normalized.contains("active") ||
            normalized.contains("suspend") || normalized.contains("work") ->
            if (current) {
                HistoryOutcome("● Open now", SemanticState.POSITIVE)
            } else {
                HistoryOutcome("● Open", SemanticState.DIM)
            }
        else -> HistoryOutcome("· Unknown", SemanticState.DIM)
    }
}

/**
 * Deterministic absolute time of one durable row (`2023-11-14 22:13 UTC`).
 * UTC is used so the rendered text (and therefore the pinned render digest)
 * never depends on the host timezone; a legacy daemon that serves no time
 * says so instead of inventing one.
 */
internal fun historyTimeText(createdMs: Long?): String {
    if (createdMs == null || createdMs <= 0L) return "time not reported"
    val formatter = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm")
    return formatter.format(Instant.ofEpochMilli(createdMs).atZone(ZoneOffset.UTC)) + " UTC"
}

/** The compact absolute date a 240px row can carry; the tooltip has the rest. */
internal fun historyRowTimeText(createdMs: Long?): String {
    if (createdMs == null || createdMs <= 0L) return "time not reported"
    return DateTimeFormatter.ofPattern("yyyy-MM-dd")
        .format(Instant.ofEpochMilli(createdMs).atZone(ZoneOffset.UTC))
}


class HistoryPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onOpenSession(sessionId: String)

        fun onRefresh()

        fun onRename(sessionId: String, title: String) {}

        fun onPin(sessionId: String, pinned: Boolean) {}

        fun onArchive(sessionId: String) {}

        fun onFork(sessionId: String) {}

        fun onExport(sessionId: String) {}
    }

    private val all = ArrayList<NativeSessionSummary>()

    private val model = DefaultListModel<NativeSessionSummary>()

    private val list = JList(model)

    private val status = WrappedLabel("prior work: -")

    private val searchField = javax.swing.JTextField("", 14)

    private val findField = javax.swing.JTextField("", 14)

    private val findButton = secondaryButton("Find in transcript")

    private val findStatus = wrappedMutedLabel(
        "Find searches durable pages, not only the rendered rows."
    )

    private var transcriptSearchListener: ((String) -> Unit)? = null

    private val renameButton = secondaryButton("Rename")

    private val pinButton = secondaryButton("Pin")

    private val archiveButton = secondaryButton("Archive")

    private val forkButton = secondaryButton("Fork")

    private val exportButton = secondaryButton("Export")

    private val openButton = primaryButton("Open")

    private val refreshButton = secondaryButton("Refresh")

    /** T31 copy (state matrix, verbatim). */
    private val unavailableHeadline = sectionHeader("History unavailable")

    private val unavailableCopy = wrappedMutedLabel("The conversation list could not be read.")

    private val unavailableDetail = wrappedMutedLabel("")

    private val refreshHistoryButton = secondaryButton("Refresh history")

    /** The card body swaps ONE visible surface: list or unavailable state. */
    private val contentCards = JPanel(java.awt.CardLayout())

    private val contentLayout = contentCards.layout as java.awt.CardLayout

    private var listener: Listener? = null

    private var available = true

    private var reason: String? = null

    private var currentSessionId: String? = null

    init {
        list.selectionMode = ListSelectionModel.SINGLE_SELECTION
        // Two-line rows: title first, date + outcome second, so a 240px tool
        // window never truncates the outcome.
        RowRhythm.install(list, lines = 2)
        list.cellRenderer = HistoryCellRenderer()
        list.addListSelectionListener { updateButtons() }
        list.a11y("Prior work", "Durable conversations. Press Enter to open the selection.")
        installActivateKey(list, "faktor.openHistorySession") { submitOpen() }
        openButton.addActionListener { submitOpen() }
        findField.a11y("Find in transcript", "Search all durable message pages, including unloaded history.")
        findField.addActionListener { runTranscriptSearch() }
        findButton.addActionListener { runTranscriptSearch() }
        renameButton.addActionListener {
            val selected = list.selectedValue?.id ?: return@addActionListener
            listener?.onRename(selected, searchField.text.trim())
        }
        pinButton.addActionListener {
            val selected = list.selectedValue?.id ?: return@addActionListener
            listener?.onPin(selected, true)
        }
        archiveButton.addActionListener {
            val selected = list.selectedValue?.id ?: return@addActionListener
            listener?.onArchive(selected)
        }
        forkButton.addActionListener {
            val selected = list.selectedValue?.id ?: return@addActionListener
            listener?.onFork(selected)
        }
        exportButton.addActionListener {
            val selected = list.selectedValue?.id ?: return@addActionListener
            listener?.onExport(selected)
        }
        openButton.a11y("Open conversation", "Opens the selected durable conversation.")
        refreshButton.addActionListener { listener?.onRefresh() }
        refreshHistoryButton.addActionListener { listener?.onRefresh() }
        searchField.toolTipText = "Filter prior work by title"
        searchField.a11y("Filter prior work", "Filters the conversation listing by title.")
        searchField.document.addDocumentListener(
            object : javax.swing.event.DocumentListener {
                override fun insertUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
                override fun removeUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
                override fun changedUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
            }
        )
        // Narrow-host fix: the label sits ABOVE the field (no fixed key
        // column that could squeeze the input at 240px).
        val searchRow = pageColumn(gap = Spacing.XS, padding = 0)
        searchRow.add(mutedLabel("Filter by title"))
        searchRow.add(searchField)
        searchRow.add(mutedLabel("Find in transcript (all pages)"))
        searchRow.add(findField)
        searchRow.add(findButton)
        searchRow.add(findStatus)
        val listContent = JPanel(BorderLayout(0, Spacing.S))
        listContent.isOpaque = false
        listContent.add(searchRow, BorderLayout.NORTH)
        listContent.add(insetScroll(list), BorderLayout.CENTER)
        val listActions = wrapRow(
            openButton, refreshButton, renameButton, pinButton, archiveButton, forkButton, exportButton
        )
        listContent.add(listActions, BorderLayout.SOUTH)
        val unavailableBody = pageColumn(gap = Spacing.XS, padding = 0)
        unavailableBody.add(unavailableHeadline)
        unavailableBody.add(unavailableCopy)
        unavailableBody.add(unavailableDetail)
        unavailableBody.add(vSpace(Spacing.XS))
        unavailableBody.add(actionRow(refreshHistoryButton))
        contentCards.isOpaque = false
        contentCards.add(listContent, "list")
        contentCards.add(unavailableBody, "unavailable")
        val sessionsBody = pageColumn(gap = Spacing.S, padding = 0)
        sessionsBody.add(contentCards)
        val body = pageColumn()
        body.add(card("Prior work", sessionsBody))
        add(panelHeader(status), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        contentLayout.show(contentCards, "list")
        updateButtons()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** D-03: transcript Find is a separate operation from title filtering. */
    fun setTranscriptSearchListener(value: ((String) -> Unit)?) {
        transcriptSearchListener = value
    }

    fun findQueryForTest(): String = findField.text

    fun submitFindForTest(query: String) {
        findField.text = query
        runTranscriptSearch()
    }

    private fun runTranscriptSearch() {
        val query = findField.text.trim()
        if (query.isEmpty()) {
            findStatus.text = "Find needs a query; nothing was searched."
            findStatus.foreground = semanticForeground(SemanticState.WARNING)
            return
        }
        transcriptSearchListener?.invoke(query)
        findStatus.text = "Find requested across durable pages: $query"
        findStatus.foreground = mutedForeground()
    }

    /** Replaces the durable listing; a still-served selection is preserved. */
    fun update(sessions: List<NativeSessionSummary>, currentId: String?) {
        val unchanged = available && sessions == all && currentId == currentSessionId
        available = true
        reason = null
        currentSessionId = currentId
        if (unchanged) {
            updateButtons()
            // The current-row marker is session-owned: repaint without a
            // model reset (selection, scroll and focus survive).
            list.repaint()
            return
        }
        val selectedId = list.selectedValue?.id
        all.clear()
        all.addAll(sessions)
        contentLayout.show(contentCards, "list")
        applyFilter(selectedId)
        revalidate()
        repaint()
    }

    fun setUnavailable(detailText: String) {
        if (!available && reason == detailText) return
        available = false
        reason = detailText
        model.clear()
        unavailableDetail.fullText = detailText
        unavailableDetail.foreground = mutedForeground()
        contentLayout.show(contentCards, "unavailable")
        updateButtons()
        revalidate()
        repaint()
    }

    /** The live list model (perf harness assertion hook). */
    internal fun listForTest(): JList<NativeSessionSummary> = list

    /** One bounded human row: title · time · outcome. */
    private fun sessionLabel(session: NativeSessionSummary): String {
        val title = session.title.trim().ifEmpty { "Untitled conversation" }
        val outcome = historyOutcomeOf(session.state, session.id == currentSessionId)
        return "${bound(title, 80)} · ${historyTimeText(session.createdMs)} · ${outcome.label}"
    }

    fun count(): Int = model.size()

    fun totalCount(): Int = all.size

    fun available(): Boolean = available

    fun label(index: Int): String = sessionLabel(model.getElementAt(index))

    /** The semantic tone of one rendered row (host-matrix legibility check). */
    internal fun outcomeTone(index: Int): SemanticState =
        historyOutcomeOf(
            model.getElementAt(index).state,
            model.getElementAt(index).id == currentSessionId
        ).tone

    fun select(index: Int) {
        list.selectedIndex = index
    }

    fun selectedId(): String? = list.selectedValue?.id

    fun openEnabled(): Boolean = openButton.isEnabled

    fun statusText(): String = status.fullText

    /** T31 surface observables (smoke copy pins). */
    fun unavailableHeadlineText(): String = unavailableHeadline.fullText

    fun unavailableCopyText(): String = unavailableCopy.fullText

    fun refreshHistoryActionText(): String = refreshHistoryButton.text

    /** The T31 refresh control (smoke click hook). */
    internal fun refreshHistoryButtonForTest(): javax.swing.JButton = refreshHistoryButton

    fun filterText(): String = searchField.text.trim()

    /** Test/composer hook: types into the search field without a display. */
    fun setFilterForTest(text: String) {
        searchField.text = text
    }

    /** Programmatic open, exactly like a click on Open. */
    fun submitOpen() {
        val session = list.selectedValue
        if (session != null) listener?.onOpenSession(session.id)
    }

    private fun applyFilter(preserveId: String? = list.selectedValue?.id) {
        val needle = searchField.text.trim().lowercase()
        val visible = all.filter { session ->
            val title = session.title.ifEmpty { "Untitled conversation" }
            needle.isEmpty() || title.lowercase().contains(needle)
        }
        // Keyed by durable id: an identical snapshot cannot reset selection
        // or scroll; only genuinely changed rows fire list events.
        KeyedListSync.sync(model, visible) { it.id }
        val preferred = preserveId ?: currentSessionId
        if (preferred != null) {
            for (i in 0 until model.size()) {
                if (model.getElementAt(i).id == preferred) {
                    list.selectedIndex = i
                    break
                }
            }
        }
        if (list.selectedIndex < 0 && model.size() > 0) {
            list.selectedIndex = 0
        }
        updateButtons()
    }

    /**
     * The REAL list path renders the same bounded human row as [label]
     * (never the raw DTO toString), with the outcome tone carried by the
     * text and the theme's semantic foreground.
     */
    private inner class HistoryCellRenderer : javax.swing.DefaultListCellRenderer() {
        override fun getListCellRendererComponent(
            listComponent: JList<*>?,
            value: Any?,
            index: Int,
            selected: Boolean,
            focus: Boolean
        ): java.awt.Component {
            super.getListCellRendererComponent(listComponent, value, index, selected, focus)
            if (!selected) {
                RowRhythm.hoverBackground(listComponent, index)?.let { background = it }
            }
            val session = value as? NativeSessionSummary ?: return this
            val title = session.title.trim().ifEmpty { "Untitled conversation" }
            val outcome = historyOutcomeOf(session.state, session.id == currentSessionId)
            text = "<html><div>" + escapeHtml(bound(title, 72)) + "</div>" +
                "<div>" + historyRowTimeText(session.createdMs) + " · " +
                escapeHtml(outcome.label) + "</div></html>"
            toolTipText = sessionLabel(session)
            if (!selected && outcome.tone != SemanticState.DIM) {
                foreground = semanticForeground(outcome.tone)
            }
            return this
        }
    }

    private fun updateButtons() {
        openButton.isEnabled = available && list.selectedValue != null
        status.text = if (!available) {
            "prior work: unavailable ($reason)"
        } else if (filterText().isNotEmpty()) {
            "prior work: ${model.size()} of ${all.size} shown"
        } else {
            "prior work: ${plural(all.size, "session")}"
        }
    }
}
