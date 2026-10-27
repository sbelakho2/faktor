// Overview: a concise, human summary of the current run. It deliberately
// stops at the at-a-glance facts (state, goal, progress, at most three
// attention items, the planned check count and spend). Full contract, proof
// and evidence stay in Verification/Advanced, and absent sections are
// omitted instead of rendering empty placeholders. Key and value stack
// vertically so a 240px tool window stays readable.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.Component
import java.awt.Dimension
import javax.swing.BoxLayout
import javax.swing.JPanel

/**
 * Aggregate the served attention causes without inventing any: explicit task
 * blockers, child blockers, failed/owed checks, an unavailable proof and an
 * unreadable contract. The result is capped at [max] (including the
 * "+N more" overflow line).
 */
internal fun overviewAttentionItems(model: TaskTreeModel, max: Int = 3): List<String> {
    val items = ArrayList<String>()
    fun add(value: String?) {
        val text = value?.trim()?.takeIf { it.isNotEmpty() } ?: return
        val bounded = bound(text, 140)
        if (!items.contains(bounded)) items.add(bounded)
    }
    for (blocker in model.taskBlockers) add(blocker)
    for (blocker in model.blockers) add(blocker.reason)
    if (model.verification.failedChecks > 0) {
        add(plural(model.verification.failedChecks, "failed check"))
    }
    if (model.verification.owed > 0) add(plural(model.verification.owed, "owed check"))
    if (model.proof?.state == "unavailable") {
        add("Verification unavailable" + (model.proof.reason?.let { " — " + bound(it, 80) } ?: ""))
    }
    if (model.contract?.malformed != null) add("Contract unreadable")
    if (items.size <= max) return items
    val head = items.subList(0, max - 1).toMutableList()
    head.add("+" + (items.size - (max - 1)) + " more")
    return head
}

class OverviewPanel : JPanel(BorderLayout()) {

    private val header = WrappedLabel("overview: no run yet")

    private val stateChip = ScrollableColumn().apply {
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        isOpaque = false
        alignmentX = Component.LEFT_ALIGNMENT
    }

    private val goalLabel = WrappedLabel("No run yet. Describe a task in the composer.")

    private val rowHosts = LinkedHashMap<String, JPanel>()

    private val rows = LinkedHashMap<String, WrappedLabel>()

    private var appliedModel: TaskTreeModel? = null

    private var modelApplied = false

    private var appliedChip: Pair<String, SemanticState>? = null

    init {
        goalLabel.font = sectionTitleFont()
        goalLabel.foreground = textForeground()
        val summaryBody = pageColumn(gap = Spacing.S, padding = 0)
        summaryBody.add(stateChip)
        summaryBody.add(goalLabel)
        for (key in listOf("Phase", "Plan", "Agents", "Needs attention", "Checks to run", "Spend")) {
            val value = WrappedLabel("")
            value.font = uiPanelFont()
            value.foreground = textForeground()
            rows[key] = value
            val host = row(key, value)
            rowHosts[key] = host
            summaryBody.add(host)
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
        if (modelApplied && appliedModel == model) return
        modelApplied = true
        appliedModel = model
        if (model == null) {
            header.text = "overview: no run yet"
            setChip("No run yet", SemanticState.DIM)
            goalLabel.fullText = "No run yet. Describe a task in the composer."
            for (key in rowHosts.keys) showRow(key, null)
            refreshChip()
            return
        }
        header.text = "overview: " + humanState(stateLabel(model))
        setChip(stateLabel(model), runStateOf(model))
        goalLabel.fullText = model.goal.ifBlank { "No goal text served for this run." }

        showRow("Phase", model.phase.takeIf { it.isNotBlank() }?.let { humanState(it) })

        val done = model.steps.count { it.state == "done" }
        showRow(
            "Plan",
            if (model.steps.isEmpty()) null
            else humanCount(done, model.steps.size, "step") + " done"
        )

        val active = model.children.count { it.state.lowercase().contains("run") }
        val blocked = model.children.count { it.blocker != null }
        showRow(
            "Agents",
            if (model.children.isEmpty()) null
            else "${plural(model.children.size, "agent")} · $active active · $blocked blocked"
        )

        val attention = overviewAttentionItems(model)
        showRow("Needs attention", attention.takeIf { it.isNotEmpty() }?.joinToString(" · "))

        val planned = model.verification.checksPlanned
        showRow("Checks to run", if (planned <= 0) null else "Checks to run · $planned")

        val spend = model.spend
        val spent = spend?.spentCostMicro
        showRow(
            "Spend",
            if (spend == null || spent == null) null
            else "$" + FormattedValues.usd(spent) +
                (spend.maxCostMicro?.let { " of $" + FormattedValues.usd(it) } ?: "") +
                " · " + (spend.spentTokens?.let { grouped(it) + " tokens" } ?: "tokens unavailable") +
                (spend.maxTokens?.let { " of " + grouped(it) } ?: "")
        )
        refreshChip()
    }

    /** Shows one row with a value, or hides it entirely when absent. */
    private fun showRow(key: String, value: String?) {
        val host = rowHosts[key] ?: return
        val rowValue = rows[key] ?: return
        val present = value != null && value.isNotBlank()
        host.isVisible = present
        if (present) rowValue.fullText = value!! else rowValue.fullText = ""
        host.revalidate()
        host.repaint()
    }

    private fun humanCount(done: Int, total: Int, noun: String): String =
        "$done of " + plural(total, noun)

    private fun grouped(value: Long): String =
        java.text.NumberFormat.getIntegerInstance().format(value)

    private fun setChip(text: String, state: SemanticState) {
        val signature = Pair(text, state)
        if (appliedChip == signature) return
        appliedChip = signature
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

    /** The rendered value of one row, or "" when the section is absent. */
    fun summaryText(key: String): String =
        if (rowHosts[key]?.isVisible == true) rows[key]?.fullText ?: "" else ""

    fun hasSummaryRowForTest(key: String): Boolean = rowHosts[key]?.isVisible == true

    fun attentionItemsForTest(model: TaskTreeModel): List<String> = overviewAttentionItems(model)

    /** The semantic tone of the run chip for one model (smoke assertions). */
    internal fun runStateOfForTest(model: TaskTreeModel): SemanticState = runStateOf(model)

    fun stateLabelText(): String {
        val chip = stateChip.components.firstOrNull() as? javax.swing.JLabel
        return chip?.text ?: ""
    }
}
