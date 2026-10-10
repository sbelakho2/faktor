// Compare approaches: the durable candidate comparison of a session run.
// The primary surface is human — one card per approach with its verification
// and independent-review outcome, the measured cost in currency and the wall
// time, plus a winner recommendation. The raw tournament machinery (manual
// id load, start form, candidate table with worktree/base revision/micro
// cost/wall milliseconds, abort reason) lives under the Advanced disclosure,
// so the normal flow never asks for a tournament id. Pure presentation:
// decide/abort/load/start are delegated to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeTournamentSummary
import java.awt.BorderLayout
import java.awt.Component
import java.math.BigInteger
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JSpinner
import javax.swing.JTable
import javax.swing.JTextArea
import javax.swing.JToggleButton
import javax.swing.JTextField
import javax.swing.SpinnerNumberModel
import javax.swing.table.AbstractTableModel
import javax.swing.table.DefaultTableCellRenderer

class TournamentPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onLoadTournament(tournamentId: String)
        fun onStartTournament(goal: String, criteria: List<String>, n: Int)
        fun onDecideTournament(tournamentId: String)
        fun onAbortTournament(tournamentId: String, reason: String)
    }

    private val title = WrappedLabel("No approaches are being compared yet.")

    private val recommendation = WrappedLabel("")

    private val summariesLabel = WrappedLabel("No saved comparisons in this session yet.")

    private val cards = ScrollableColumn()

    private val candidatesModel = CandidateTableModel()

    private val candidatesTable = JTable(candidatesModel)

    private val loadField = JTextField(10)

    private val loadButton = secondaryButton("Load")

    private val goalField = JTextField(18)

    private val criteriaField = JTextField(18)

    private val countSpinner = JSpinner(SpinnerNumberModel(2, 2, 4, 1))

    private val startButton = primaryButton("Start comparison")

    private val decideButton = primaryButton("Recommend winner")

    private val abortButton = secondaryButton("Abort")

    private val advancedToggle = JToggleButton("Show advanced")

    private val advancedBody = JPanel(BorderLayout())

    /**
     * Destructive-action confirmation seam: production asks; the host matrix
     * and smoke tests never click Abort, so the dialog cannot fire offscreen.
     */
    internal var confirmAbort: (String) -> Boolean = {
        javax.swing.JOptionPane.showConfirmDialog(
            this,
            "Abort the open comparison? Settled approaches stay, and the run can no longer be decided.",
            "Abort comparison",
            javax.swing.JOptionPane.OK_CANCEL_OPTION,
            javax.swing.JOptionPane.WARNING_MESSAGE
        ) == javax.swing.JOptionPane.OK_OPTION
    }

    private val abortReasonField = JTextField(14)

    private val detail = compactArea(4)

    private var listener: Listener? = null

    private var currentTournament: TournamentView? = null

    init {
        cards.layout = javax.swing.BoxLayout(cards, javax.swing.BoxLayout.Y_AXIS)
        candidatesTable.fillsViewportHeight = true
        TableRhythm.install(candidatesTable)
        candidatesTable.showVerticalLines = false
        candidatesTable.intercellSpacing = java.awt.Dimension(0, 1)
        candidatesTable.gridColor = cardBorderColor()
        candidatesTable.autoResizeMode = JTable.AUTO_RESIZE_OFF
        candidatesTable.setDefaultRenderer(Any::class.java, WinnerAwareRenderer())
        for (index in 0 until candidatesTable.columnCount) {
            val preferred = when (index) {
                0 -> 110
                1 -> 90
                2 -> 120
                3 -> 140
                4 -> 90
                5 -> 80
                else -> 80
            }
            candidatesTable.columnModel.getColumn(index).preferredWidth = preferred
        }
        candidatesTable.selectionModel.addListSelectionListener {
            val row = candidatesTable.selectedRow
            if (row >= 0) {
                val candidate = candidatesModel.candidateAt(row)
                detail.text = describe(candidate)
            }
        }

        title.font = sectionTitleFont()
        summariesLabel.font = uiPanelFont()
        summariesLabel.foreground = mutedForeground()
        recommendation.font = uiPanelFont()

        // Advanced: manual id load, start form, raw candidate table, abort reason.
        val loadBody = FormGrid()
            .row("Comparison id", loadField)
            .span(actionRow(loadButton))
            .build()
        loadButton.addActionListener {
            val id = loadField.text.trim()
            if (id.isEmpty()) {
                title.text = "comparison: load refused (no id entered)"
            } else {
                listener?.onLoadTournament(id)
            }
        }
        val startBody = FormGrid()
            .row("Goal", goalField)
            .row("Criteria (comma separated)", criteriaField)
            .row("Approaches", countSpinner)
            .span(actionRow(startButton))
            .build()
        countSpinner.toolTipText = "How many approaches to compare (2-4)"
        startButton.addActionListener {
            val goal = goalField.text.trim()
            val criteria = criteriaField.text.split(',')
                .map { it.trim() }
                .filter { it.isNotEmpty() }
            if (goal.isEmpty() || criteria.isEmpty()) {
                detail.text = "a comparison needs a goal and at least one criterion"
                return@addActionListener
            }
            listener?.onStartTournament(goal, criteria, (countSpinner.value as Number).toInt())
        }
        val advancedColumn = pageColumn(gap = Spacing.S, padding = 0)
        advancedColumn.add(sectionHeader("Manual id load", muted = true))
        advancedColumn.add(loadBody)
        advancedColumn.add(sectionHeader("Start a comparison", muted = true))
        advancedColumn.add(startBody)
        advancedColumn.add(sectionHeader("Candidate accounting", muted = true))
        advancedColumn.add(card(null, insetTableScroll(candidatesTable)))
        advancedColumn.add(sectionHeader("Candidate detail", muted = true))
        advancedColumn.add(insetScroll(detail))
        advancedColumn.add(actionRow(abortButton))
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

        // Decision controls stay primary: deciding an open comparison and
        // aborting it are run-level actions, not diagnostics.
        decideButton.isEnabled = false
        decideButton.addActionListener {
            val id = currentTournament?.id
            if (id != null && decideButton.isEnabled) listener?.onDecideTournament(id)
        }
        abortButton.isEnabled = false
        abortButton.addActionListener {
            val id = currentTournament?.id
            if (id != null && abortButton.isEnabled && confirmAbort(abortReasonField.text.trim())) {
                listener?.onAbortTournament(id, abortReasonField.text.trim())
            }
        }
        val controlBody = FormGrid()
            .row("Abort reason", abortReasonField)
            .span(actionRow(decideButton))
            .build()

        val summaryBody = pageColumn(gap = Spacing.XS, padding = 0)
        summaryBody.add(title)
        summaryBody.add(recommendation)
        summaryBody.add(vSpace(Spacing.S))
        summaryBody.add(cards)

        val body = pageColumn()
        body.add(card("Compare approaches", summaryBody))
        body.add(vSpace(Spacing.M))
        body.add(card("Decision", controlBody))
        body.add(vSpace(Spacing.M))
        body.add(advancedToggle)
        body.add(vSpace(Spacing.XS))
        body.add(advancedBody)
        add(pageScroll(body), BorderLayout.CENTER)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** The durable listing of the session's comparisons (newest last). */
    fun setSummaries(summaries: List<NativeTournamentSummary>) {
        if (summaries.isEmpty()) {
            summariesLabel.text = "No saved comparisons in this session yet."
            return
        }
        summariesLabel.text = "saved comparisons: " + summaries.joinToString(", ") {
            it.id + "[" + it.state + "]"
        }
    }

    /** Renders (or clears) the comparison; null means "no comparison exists". */
    fun setTournament(tournament: TournamentView?) {
        currentTournament = tournament
        decideButton.isEnabled = tournament != null && tournament.canDecide
        abortButton.isEnabled = tournament != null && tournament.open
        cards.removeAll()
        if (tournament == null) {
            title.text = "No approaches are being compared yet."
            recommendation.fullText = ""
            candidatesModel.setCandidates(emptyList())
            detail.text = ""
            refreshCards()
            return
        }
        title.text = buildString {
            append(plural(tournament.candidates.size, "approach", "approaches"))
            append(" · ").append(tournament.state)
            if (tournament.criteria.isNotEmpty()) {
                append(" · ").append(plural(tournament.criteria.size, "criterion", "criteria"))
            }
        }
        val winnerIndex = tournament.winner?.let { winner ->
            tournament.candidates.indexOfFirst { it.childId == winner }.takeIf { it >= 0 }
        }
        recommendation.fullText = if (winnerIndex == null) {
            if (tournament.open) {
                "No winner yet. The comparison is still running; a recommendation appears when every approach settles."
            } else {
                "No winner was proposed for this comparison."
            }
        } else {
            val candidate = tournament.candidates[winnerIndex]
            "Recommended: Approach ${winnerIndex + 1} — " + outcomeText(candidate) +
                ". Recommendations are proposals; integration stays the explicit approved-merge path."
        }
        recommendation.foreground = if (winnerIndex == null) {
            mutedForeground()
        } else {
            semanticForeground(SemanticState.POSITIVE)
        }
        candidatesModel.setCandidates(tournament.candidates)
        for ((index, candidate) in tournament.candidates.withIndex()) {
            cards.add(vSpace(Spacing.S))
            cards.add(candidateCard(index, candidate, candidate.winner))
        }
        refreshCards()
    }

    private fun refreshCards() {
        cards.revalidate()
        cards.repaint()
        revalidate()
        repaint()
    }

    /** One human approach card: label, outcome, cost and duration. */
    private fun candidateCard(index: Int, candidate: TournamentCandidateView, winner: Boolean): JPanel {
        val body = pageColumn(gap = Spacing.XS, padding = 0)
        // A plain label: "Approach 1" must stay on one line at every width.
        val label = JLabel("Approach ${index + 1}")
        label.font = sectionTitleFont()
        label.foreground = textForeground()
        body.add(label)
        if (winner) {
            // The recommendation badge stacks under the heading: side-by-side
            // it overlapped the title at 240px.
            val badge = JLabel("Recommended")
            badge.font = sectionTitleFont()
            badge.foreground = semanticForeground(SemanticState.POSITIVE)
            body.add(badge)
        }
        body.add(wrappedMutedLabel(outcomeText(candidate)))
        body.add(
            wrappedMutedLabel(
                "Cost " + currency(candidate.costMicro) + " · took " + humanDuration(candidate.wallMs) +
                    " · state " + candidate.state
            )
        )
        return card(null, body, hgap = Spacing.S, vgap = Spacing.S)
    }

    /** The human outcome line: verification + independent review. */
    private fun outcomeText(candidate: TournamentCandidateView): String {
        val verification = when {
            candidate.verification == null -> "verification not recorded"
            candidate.verificationPass == true -> "verification passed"
            else -> "verification did not pass"
        }
        val review = candidate.reviewRank?.let { rank ->
            "independent review " + rank + (candidate.reviewer?.let { " by $it" } ?: "")
        } ?: "no independent review yet"
        return "$verification · $review"
    }

    fun current(): TournamentView? = currentTournament

    /** The theme-derived winner foreground (smoke legibility observability). */
    internal fun winnerForeground(): java.awt.Color = semanticForeground(SemanticState.POSITIVE)

    fun decideEnabled(): Boolean = decideButton.isEnabled

    fun abortEnabled(): Boolean = abortButton.isEnabled

    fun summariesText(): String = summariesLabel.fullText

    fun abortReason(): String = abortReasonField.text.trim()

    fun candidateCount(): Int = candidatesModel.rowCount

    fun loadedId(): String = loadField.text.trim()

    fun startGoal(): String = goalField.text.trim()

    fun startCriteria(): List<String> = criteriaField.text.split(',')
        .map { it.trim() }
        .filter { it.isNotEmpty() }

    fun startCount(): Int = (countSpinner.value as Number).toInt()

    /** The rendered primary text of the comparison surface (smoke observable). */
    fun primaryText(): String = buildString {
        append(title.fullText).append('\n').append(recommendation.fullText).append('\n')
        for (index in 0 until cards.componentCount) {
            val component = cards.getComponent(index)
            if (component is JPanel) appendCardText(component, this)
        }
    }

    private fun appendCardText(component: Component, out: StringBuilder) {
        if (component is WrappedLabel) out.append(component.fullText).append('\n')
        if (component is java.awt.Container) {
            for (child in component.components) appendCardText(child, out)
        }
    }

    private fun describe(candidate: TournamentCandidateView?): String {
        if (candidate == null) return ""
        val text = StringBuilder()
        text.append("approach: ").append(candidate.childId)
        text.append("\nstate: ").append(candidate.state)
        text.append("\nworktree: ").append(candidate.worktree.ifEmpty { "-" })
        text.append("\nbase revision: ").append(candidate.baseRevision.ifEmpty { "-" })
        text.append("\nverification: ")
        if (candidate.verification == null) {
            text.append("none")
        } else {
            text.append("#").append(candidate.verification)
            text.append(if (candidate.verificationPass == true) " passed" else " not passed")
        }
        text.append("\nreview: ").append(candidate.reviewRank ?: "none")
        if (candidate.reviewer != null) text.append(" by ").append(candidate.reviewer)
        text.append("\ncost micro: ").append(candidate.costMicro)
        text.append("  wall ms: ").append(candidate.wallMs)
        if (candidate.winner) text.append("\nWINNER (proposed; not auto-integrated)")
        return text.toString()
    }

    private class CandidateTableModel : AbstractTableModel() {

        private val columns = arrayOf(
            "candidate", "state", "verification", "review", "cost micro", "wall ms", "winner"
        )

        private var candidates: List<TournamentCandidateView> = emptyList()

        fun setCandidates(value: List<TournamentCandidateView>) {
            candidates = value
            fireTableDataChanged()
        }

        fun candidateAt(row: Int): TournamentCandidateView? =
            if (row in candidates.indices) candidates[row] else null

        override fun getRowCount(): Int = candidates.size

        override fun getColumnCount(): Int = columns.size

        override fun getColumnName(column: Int): String = columns[column]

        override fun isCellEditable(row: Int, column: Int): Boolean = false

        override fun getValueAt(row: Int, column: Int): Any {
            val candidate = candidates[row]
            return when (column) {
                0 -> candidate.childId
                1 -> candidate.state
                2 -> when {
                    candidate.verification == null -> "-"
                    candidate.verificationPass == true -> "#${candidate.verification} pass"
                    else -> "#${candidate.verification} fail"
                }
                3 -> if (candidate.reviewRank == null) {
                    "-"
                } else {
                    candidate.reviewRank + (candidate.reviewer?.let { " ($it)" } ?: "")
                }
                4 -> candidate.costMicro
                5 -> candidate.wallMs
                else -> if (candidate.winner) "WINNER" else ""
            }
        }
    }

    private class WinnerAwareRenderer : DefaultTableCellRenderer() {
        override fun getTableCellRendererComponent(
            table: JTable?,
            value: Any?,
            isSelected: Boolean,
            hasFocus: Boolean,
            row: Int,
            column: Int
        ): java.awt.Component {
            val component = super.getTableCellRendererComponent(
                table, value, isSelected, hasFocus, row, column
            )
            toolTipText = value?.toString()
            if (!isSelected && table != null && TableRhythm.isHovered(table, row)) {
                component.background = FaktorTheme.hover()
            }
            val winnerColumn = table != null &&
                table.model.getColumnName(table.convertColumnIndexToModel(column)) == "winner"
            val winner = winnerColumn && value == "WINNER"
            if (winner) {
                // Theme token, never raw RGB; "WINNER" text keeps the state
                // distinguishable without color.
                component.foreground = semanticForeground(SemanticState.POSITIVE)
                component.font = component.font.deriveFont(java.awt.Font.BOLD)
            } else {
                component.foreground = if (isSelected) table?.selectionForeground else table?.foreground
                component.font = component.font.deriveFont(java.awt.Font.PLAIN)
            }
            return component
        }
    }
}

/** Exact currency of one micro-USD amount (never a rounded float). */
internal fun currency(value: BigInteger): String = "$" + MicroMoney.usdText(value)

/** Human duration: milliseconds under a second, then one decimal second, then minutes. */
internal fun humanDuration(millis: Long): String {
    if (millis < 1000L) return "$millis ms"
    if (millis < 60_000L) {
        val seconds = millis / 1000.0
        return String.format(java.util.Locale.ROOT, "%.1f s", seconds)
    }
    val minutes = millis / 60_000L
    val seconds = (millis % 60_000L) / 1000L
    return "$minutes m " + (if (seconds < 10) "0" else "") + seconds + " s"
}
