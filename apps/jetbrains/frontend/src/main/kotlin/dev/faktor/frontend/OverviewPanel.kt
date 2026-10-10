// Overview: the current run at a glance. Renders the plan-derived model as
// human summary rows (state, goal, phase, plan progress, agents, verification,
// acceptance criteria, spend and finish actions) instead of a raw dump. Key
// and value stack vertically so a 240px tool window stays readable; the
// blocking/approval cards stay on their own actionable surfaces.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import java.awt.BorderLayout
import java.awt.Component
import java.awt.Dimension
import javax.swing.BoxLayout
import javax.swing.JPanel

class OverviewPanel : JPanel(BorderLayout()) {

    private val header = WrappedLabel("overview: no run yet")

    private val stateChip = ScrollableColumn().apply {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        isOpaque = false
        alignmentX = Component.LEFT_ALIGNMENT
    }

    private val goalLabel = WrappedLabel("No run yet. Describe a task in the composer.")

    private val rows = LinkedHashMap<String, WrappedLabel>()

    init {
        goalLabel.font = sectionTitleFont()
        goalLabel.foreground = textForeground()
        val summaryBody = pageColumn(gap = Spacing.S, padding = 0)
        summaryBody.add(stateChip)
        summaryBody.add(goalLabel)
        for (key in listOf(
            "Phase", "Plan", "Agents", "Verification", "Acceptance criteria", "Spend", "Finish actions"
        )) {
            val value = WrappedLabel("—")
            value.font = uiPanelFont()
            value.foreground = textForeground()
            rows[key] = value
            summaryBody.add(row(key, value))
        }
        val body = pageColumn()
        body.add(card("Current run", summaryBody))
        add(panelHeader(header), BorderLayout.NORTH)
        add(pageScroll(body), BorderLayout.CENTER)
    }

    /**
     * One stacked key/value row: the muted key sits above the wrapping value,
     * so no fixed key column can squeeze the value to a sliver at 240px.
     */
    private fun row(key: String, value: WrappedLabel): JPanel {
        val panel = JPanel(BorderLayout(0, 2))
        panel.isOpaque = false
        panel.alignmentX = Component.LEFT_ALIGNMENT
        panel.maximumSize = Dimension(Int.MAX_VALUE, panel.maximumSize.height)
        val label = mutedLabel(key)
        label.alignmentX = Component.LEFT_ALIGNMENT
        panel.add(label, BorderLayout.NORTH)
        panel.add(value, BorderLayout.CENTER)
        return panel
    }

    /** Replaces the view with the served model; null renders the honest empty state. */
    fun updateModel(model: TaskTreeModel?) {
        if (model == null) {
            header.text = "overview: no run yet"
            setChip("No run yet", SemanticState.DIM)
            goalLabel.fullText = "No run yet. Describe a task in the composer."
            for (value in rows.values) value.fullText = "—"
            refreshChip()
            return
        }
        header.text = "overview: " + humanState(stateLabel(model))
        setChip(stateLabel(model), runStateOf(model))
        goalLabel.fullText = model.goal.ifBlank { "No goal text served for this run." }
        rows["Phase"]?.fullText = humanState(model.phase.ifBlank { "—" })
        val done = model.steps.count { it.state == "done" }
        rows["Plan"]?.fullText = if (model.steps.isEmpty()) {
            "No plan steps served yet"
        } else {
            "$done of ${model.steps.size} steps done"
        }
        val active = model.children.count { it.state.lowercase().contains("run") }
        val blocked = model.children.count { it.blocker != null }
        rows["Agents"]?.fullText = when {
            model.children.isEmpty() -> "No agents on this run yet"
            else -> "${plural(model.children.size, "agent")} · $active active · $blocked blocked"
        }
        val verification = model.verification
        rows["Verification"]?.fullText = buildString {
            append(if (verification.verified) "Verified" else "Not verified yet")
            append(" · criteria ")
            append(verification.criteriaPassed).append(" of ").append(verification.criteriaTotal)
            append(" passed · ").append(plural(verification.failedChecks, "failed check"))
            append(" · ").append(plural(verification.owed, "owed check"))
            verification.reviewer?.let { append(" · reviewer ").append(it) }
        }
        rows["Acceptance criteria"]?.fullText = if (model.acceptanceCriteria.isEmpty()) {
            "No acceptance criteria"
        } else {
            model.acceptanceCriteria.joinToString(" · ") { bound(it, 120) }
        }
        val spend = model.spend
        val spent = spend?.spentCostMicro
        rows["Spend"]?.fullText = if (spend == null || spent == null) {
            "No spend recorded yet"
        } else {
            "$" + MicroMoney.usdText(spent) +
                (spend.maxCostMicro?.let { " of $" + MicroMoney.usdText(it) } ?: "") +
                " · " + (spend.spentTokens ?: 0L) + " tokens" +
                (spend.maxTokens?.let { " of $it" } ?: "")
        }
        val completion = model.completion
        rows["Finish actions"]?.fullText = if (completion == null) {
            "No finish actions recorded yet"
        } else {
            completion.steps.joinToString(" · ") { "${it.step} ${it.status}" }
        }
        refreshChip()
    }

    private fun setChip(text: String, state: SemanticState) {
        stateChip.removeAll()
        // A plain tone-colored label rather than a bordered chip: the state
        // must stay legible in every offscreen/theme render, and the header
        // repeats it as text so tone is never the only cue.
        val label = WrappedLabel(text)
        label.font = sectionTitleFont()
        label.foreground = if (state == SemanticState.DIM) {
            mutedForeground()
        } else {
            semanticForeground(state)
        }
        label.alignmentX = Component.LEFT_ALIGNMENT
        stateChip.add(label)
    }

    private fun refreshChip() {
        stateChip.revalidate()
        stateChip.repaint()
    }

    private fun stateLabel(model: TaskTreeModel): String {
        if (model.proof?.verified == true) return "Verified"
        return model.state.ifBlank { "Unknown" }
    }

    private fun runStateOf(model: TaskTreeModel): SemanticState {
        if (model.proof?.verified == true) return SemanticState.POSITIVE
        val state = model.state.lowercase()
        return when {
            state.contains("fail") || state.contains("error") -> SemanticState.NEGATIVE
            model.blockers.isNotEmpty() || model.taskBlockers.isNotEmpty() ->
                SemanticState.WARNING
            state.contains("block") || state.contains("await") || state.contains("needs") ->
                SemanticState.WARNING
            state.contains("complete") || state.contains("done") -> SemanticState.POSITIVE
            else -> SemanticState.DIM
        }
    }

    /** "in_progress" reads as "In progress"; already-sentence states pass through. */
    private fun humanState(state: String): String {
        if (state.isEmpty()) return state
        if (!state.contains('_') && !state.contains('-')) return state
        val words = state.replace('_', ' ').replace('-', ' ')
        return words.substring(0, 1).uppercase() + words.substring(1)
    }

    fun summaryText(key: String): String = rows[key]?.fullText ?: ""

    fun stateLabelText(): String {
        val chip = stateChip.components.firstOrNull() as? javax.swing.JLabel
        return chip?.text ?: ""
    }
}
