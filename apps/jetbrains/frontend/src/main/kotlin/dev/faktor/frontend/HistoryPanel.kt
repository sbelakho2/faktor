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
    }

    private val all = ArrayList<NativeSessionSummary>()

    private val model = DefaultListModel<NativeSessionSummary>()

    private val list = JList(model)

    private val status = WrappedLabel("prior work: -")

    private val searchField = javax.swing.JTextField("", 14)

    private val openButton = primaryButton("Open")

    private val refreshButton = secondaryButton("Refresh")

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
        openButton.addActionListener { submitOpen() }
        refreshButton.addActionListener { listener?.onRefresh() }
        searchField.toolTipText = "Filter prior work by title"
        searchField.document.addDocumentListener(
            object : javax.swing.event.DocumentListener {
                override fun insertUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
                override fun removeUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
                override fun changedUpdate(e: javax.swing.event.DocumentEvent?) = applyFilter()
            }
        )
        val searchRow = JPanel(BorderLayout(Spacing.S, 0))
        searchRow.isOpaque = false
        searchRow.add(mutedLabel("Search"), BorderLayout.WEST)
        searchRow.add(searchField, BorderLayout.CENTER)
        val sessionsBody = JPanel(BorderLayout(0, Spacing.S))
        sessionsBody.isOpaque = false
        sessionsBody.add(searchRow, BorderLayout.NORTH)
        sessionsBody.add(insetScroll(list), BorderLayout.CENTER)
        sessionsBody.add(actionRow(openButton, refreshButton), BorderLayout.SOUTH)
        val body = pageColumn()
        body.add(card("Prior work", sessionsBody))
        add(panelHeader(status), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
        updateButtons()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** Replaces the durable listing; a still-served selection is preserved. */
    fun update(sessions: List<NativeSessionSummary>, currentId: String?) {
        available = true
        reason = null
        currentSessionId = currentId
        val selectedId = list.selectedValue?.id
        all.clear()
        all.addAll(sessions)
        applyFilter(selectedId)
    }

    fun setUnavailable(detailText: String) {
        available = false
        reason = detailText
        model.clear()
        updateButtons()
    }

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
        model.clear()
        for (session in all) {
            val title = session.title.ifEmpty { "Untitled conversation" }
            if (needle.isNotEmpty() && !title.lowercase().contains(needle)) continue
            model.addElement(session)
        }
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
