// Tournament view: the durable candidates of a session tournament with
// their verification record/verdict, independent review, measured cost and
// wall time, and the winner state. Auto-populated from the durable listing
// (`GET .../tournaments`) on session open; supports loading a tournament by
// id, deciding/aborting an OPEN tournament through the additive native
// control routes (decide is disabled until every candidate settled) and
// starting an N = 2..4 candidate tournament. Pure presentation: work is
// delegated to a Listener.
package dev.faktor.frontend

import dev.faktor.shared.NativeTournamentSummary
import java.awt.BorderLayout
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JSpinner
import javax.swing.JTable
import javax.swing.JTextArea
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

    private val title = WrappedLabel("tournament: none on this session")

    private val summariesLabel = WrappedLabel("session tournaments: none")

    private val candidatesModel = CandidateTableModel()

    private val candidatesTable = JTable(candidatesModel)

    private val loadField = JTextField(10)

    private val loadButton = secondaryButton("Load")

    private val goalField = JTextField(18)

    private val criteriaField = JTextField(18)

    private val countSpinner = JSpinner(SpinnerNumberModel(2, 2, 4, 1))

    private val startButton = primaryButton("Start tournament")

    private val decideButton = primaryButton("Decide winner")

    private val abortButton = secondaryButton("Abort")

    private val abortReasonField = JTextField(14)

    private val detail = compactArea(4)

    private var listener: Listener? = null

    private var currentTournament: TournamentView? = null

    init {
        candidatesTable.fillsViewportHeight = true
        candidatesTable.rowHeight = Math.max(
            candidatesTable.rowHeight,
            candidatesTable.getFontMetrics(uiPanelFont()).height + 8
        )
        candidatesTable.showVerticalLines = false
        candidatesTable.intercellSpacing = java.awt.Dimension(0, 1)
        candidatesTable.gridColor = cardBorderColor()
        // Natural column widths with horizontal scrolling: at 240px the old
        // proportional resize shredded every heading to "c... st..."; the
        // overview keeps readable columns and each cell carries its full text
        // as a tooltip.
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

        val loadBody = FormGrid()
            .row("Tournament id", loadField)
            .span(actionRow(loadButton))
            .build()
        loadButton.addActionListener {
            val id = loadField.text.trim()
            if (id.isEmpty()) {
                title.text = "tournament: load refused (no id entered)"
            } else {
                listener?.onLoadTournament(id)
            }
        }

        val startBody = FormGrid()
            .row("Goal", goalField)
            .row("Criteria (comma separated)", criteriaField)
            .row("Candidates (2-4)", countSpinner)
            .span(actionRow(startButton))
            .build()
        startButton.addActionListener {
            val goal = goalField.text.trim()
            val criteria = criteriaField.text.split(',')
                .map { it.trim() }
                .filter { it.isNotEmpty() }
            if (goal.isEmpty() || criteria.isEmpty()) {
                detail.text = "tournament needs a goal and at least one criterion"
                return@addActionListener
            }
            listener?.onStartTournament(goal, criteria, (countSpinner.value as Number).toInt())
        }

        // Decide/abort are OPEN-only: decide additionally waits until every
        // candidate settled (the engine refuses NoEligibleWinner otherwise).
        decideButton.isEnabled = false
        abortButton.isEnabled = false
        decideButton.addActionListener {
            val id = currentTournament?.id
            if (id != null && decideButton.isEnabled) listener?.onDecideTournament(id)
        }
        abortButton.addActionListener {
            val id = currentTournament?.id
            if (id != null && abortButton.isEnabled) {
                listener?.onAbortTournament(id, abortReasonField.text.trim())
            }
        }
        val controlBody = FormGrid()
            .row("Abort reason", abortReasonField)
            .span(actionRow(decideButton, abortButton))
            .build()

        val summaryBody = JPanel(BorderLayout(0, Spacing.XS))
        summaryBody.isOpaque = false
        summaryBody.add(title, BorderLayout.NORTH)
        val loadColumn = JPanel(BorderLayout(0, Spacing.XS))
        loadColumn.isOpaque = false
        loadColumn.add(summariesLabel, BorderLayout.NORTH)
        loadColumn.add(loadBody, BorderLayout.CENTER)
        summaryBody.add(loadColumn, BorderLayout.CENTER)
        summaryBody.add(
            wrappedMutedLabel("Winners are proposed only; integration stays the explicit approved-merge path."),
            BorderLayout.SOUTH
        )

        val body = pageColumn()
        body.add(card("Tournament", summaryBody))
        body.add(vSpace(Spacing.S))
        body.add(card("Start tournament", startBody))
        body.add(vSpace(Spacing.S))
        body.add(card("Candidates", tableScroll(candidatesTable)))
        body.add(vSpace(Spacing.S))
        body.add(card("Decide / abort", controlBody))
        body.add(vSpace(Spacing.S))
        body.add(card("Candidate detail", JScrollPane(detail)))
        add(pageScroll(body), BorderLayout.CENTER)
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /** The durable listing of the session's tournaments (newest last). */
    fun setSummaries(summaries: List<NativeTournamentSummary>) {
        if (summaries.isEmpty()) {
            summariesLabel.text = "session tournaments: none"
            return
        }
        summariesLabel.text = "session tournaments: " + summaries.joinToString(", ") {
            it.id + "[" + it.state + "]"
        }
    }

    /** Renders (or clears) the tournament; null means "no tournament exists". */
    fun setTournament(tournament: TournamentView?) {
        currentTournament = tournament
        decideButton.isEnabled = tournament != null && tournament.canDecide
        abortButton.isEnabled = tournament != null && tournament.open
        if (tournament == null) {
            title.text = "tournament: none on this session"
            candidatesModel.setCandidates(emptyList())
            detail.text = ""
            return
        }
        val winner = tournament.winner ?: "-"
        title.text = "tournament ${tournament.id} [${tournament.state}] " +
            "winner=$winner criteria=${tournament.criteria.size} candidates=${tournament.candidates.size}"
        candidatesModel.setCandidates(tournament.candidates)
        if (tournament.candidates.isNotEmpty()) candidatesTable.setRowSelectionInterval(0, 0)
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

    private fun describe(candidate: TournamentCandidateView?): String {
        if (candidate == null) return ""
        val text = StringBuilder()
        text.append("candidate: ").append(candidate.childId)
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
