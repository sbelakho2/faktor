// The agent roster: every row leads with the plan-derived role/title (the
// step kind and goal the agent owns), falls back to its id, and shows the
// state plus what it is doing next. Exact identities (agent id, kind,
// model, budget, ownership, spend) stay under the Advanced disclosure. The
// operator controls (pause, resume, cancel, retry, steer, model, token
// budget, cost budget) are unchanged: one state-aware primary action plus an
// overflow menu, dispatched to the Listener; the input dialogs and every
// native call stay in FaktorChatPanel.
package dev.faktor.frontend

import dev.faktor.shared.NativeAgent
import java.awt.BorderLayout
import javax.swing.DefaultComboBoxModel
import javax.swing.JButton
import javax.swing.JComboBox
import javax.swing.JMenuItem
import javax.swing.JPanel
import javax.swing.JPopupMenu
import javax.swing.JTextArea
import javax.swing.SwingUtilities

class AgentsPanel : JPanel(BorderLayout()) {

    interface Listener {
        fun onRefresh()

        /** A control that cannot run (no agent selected) surfaces here. */
        fun onNotice(message: String) {}

        fun onPause(agent: NativeAgent) {}

        fun onResume(agent: NativeAgent) {}

        fun onCancel(agent: NativeAgent) {}

        fun onRetry(agent: NativeAgent) {}

        fun onSteer(agent: NativeAgent) {}

        fun onSetModel(agent: NativeAgent) {}

        fun onSetTokenBudget(agent: NativeAgent) {}

        fun onSetCostBudget(agent: NativeAgent) {}
    }

    private val agentsModel = DefaultComboBoxModel<NativeAgent>()

    private val agentsCombo = JComboBox(agentsModel)

    /** The agent-control buttons by label, so tests can drive the click path. */
    private val agentControlButtons = LinkedHashMap<String, JButton>()

    private val agentsArea = JTextArea(5, 32)

    private val advancedToggle = secondaryButton("Show advanced")

    private val advancedBody = JPanel(BorderLayout())

    private lateinit var primaryAction: JButton

    private lateinit var moreMenu: JPopupMenu

    private var listener: Listener? = null

    /** The plan-derived role lookup: agent id to the step it owns. */
    private var roles: Map<String, String> = emptyMap()

    /** The plan-derived spend lookup: agent id to exact micro-USD spend. */
    private var spends: Map<String, java.math.BigInteger> = emptyMap()

    init {
        val refreshAgents = secondaryButton("Refresh agents")
        refreshAgents.addActionListener { listener?.onRefresh() }
        // The combo renders one bounded human row (role — state + action);
        // the full row stays available as tooltip.
        agentsCombo.renderer = object : javax.swing.DefaultListCellRenderer() {
            override fun getListCellRendererComponent(
                list: javax.swing.JList<*>?,
                value: Any?,
                index: Int,
                selected: Boolean,
                focus: Boolean
            ): java.awt.Component {
                super.getListCellRendererComponent(list, value, index, selected, focus)
                val agent = value as? NativeAgent ?: return this
                text = bound(rowText(agent), 96)
                toolTipText = rowText(agent)
                return this
            }
        }
        // Every control still exists (the map is the dispatch source and the
        // test hook); the VISIBLE surface is one state-aware primary action
        // and an overflow menu (audit UI: no wall of peer buttons).
        agentButton("Pause") { agent -> listener?.onPause(agent) }
        agentButton("Resume") { agent -> listener?.onResume(agent) }
        agentButton("Cancel") { agent -> listener?.onCancel(agent) }
        agentButton("Retry") { agent -> listener?.onRetry(agent) }
        agentButton("Steer") { agent -> listener?.onSteer(agent) }
        agentButton("Model") { agent -> listener?.onSetModel(agent) }
        agentButton("Token budget") { agent -> listener?.onSetTokenBudget(agent) }
        agentButton("Cost budget") { agent -> listener?.onSetCostBudget(agent) }
        primaryAction = primaryButton("Steer")
        primaryAction.addActionListener {
            when (selectedAgentAction()) {
                AgentAction.Resume -> clickControl("Resume")
                AgentAction.Retry -> clickControl("Retry")
                AgentAction.Steer -> clickControl("Steer")
                AgentAction.None -> listener?.onNotice("select an agent first")
            }
        }
        val moreButton = secondaryButton("More ▾")
        moreMenu = JPopupMenu()
        for (label in listOf(
            "Pause", "Resume", "Cancel", "Retry", "Steer", "Model", "Token budget", "Cost budget"
        )) {
            val item = JMenuItem(label)
            item.addActionListener { clickControl(label) }
            moreMenu.add(item)
        }
        moreMenu.addSeparator()
        val refreshItem = JMenuItem("Refresh agents")
        refreshItem.addActionListener { listener?.onRefresh() }
        moreMenu.add(refreshItem)
        moreButton.addActionListener {
            moreMenu.show(moreButton, 0, moreButton.height)
        }
        agentsCombo.addActionListener { syncAgentControls() }
        refreshAgents.addActionListener { syncAgentControls() }
        val controls = actionRow(primaryAction, moreButton)
        val controlsBody = pageColumn(gap = Spacing.S, padding = 0)
        controlsBody.add(agentsCombo)
        controlsBody.add(controls)
        controlsBody.add(actionRow(refreshAgents))
        agentsArea.isEditable = false
        agentsArea.lineWrap = true
        agentsArea.wrapStyleWord = true
        agentsArea.font = panelMonospace()
        advancedToggle.addActionListener {
            advancedBody.isVisible = advancedToggle.isSelected
            advancedToggle.text = if (advancedToggle.isSelected) "Hide advanced" else "Show advanced"
            revalidate()
            repaint()
        }
        advancedBody.isOpaque = false
        advancedBody.add(card("Agent detail", insetScroll(agentsArea)), BorderLayout.CENTER)
        advancedBody.isVisible = false
        val page = pageColumn()
        page.add(card("Agent controls", controlsBody))
        page.add(vSpace(Spacing.M))
        page.add(advancedToggle)
        page.add(vSpace(Spacing.XS))
        page.add(advancedBody)
        add(pageScroll(page), BorderLayout.CENTER)
        syncAgentControls()
    }

    private fun panelMonospace(): java.awt.Font = monospacePanelFont()

    /**
     * Registers one control in the dispatch map. The map is the visible
     * dispatch source: visible surfaces render a subset (primary + menu),
     * while every command stays wired exactly once.
     */
    private fun agentButton(label: String, action: (NativeAgent) -> Unit) {
        val button = secondaryButton(label)
        button.addActionListener {
            val agent = agentsCombo.selectedItem as? NativeAgent ?: return@addActionListener
            action(agent)
        }
        agentControlButtons[label] = button
    }

    private enum class AgentAction { Steer, Resume, Retry, None }

    /** The state-appropriate primary action for the selected agent. */
    private fun selectedAgentAction(): AgentAction {
        val agent = agentsCombo.selectedItem as? NativeAgent ?: return AgentAction.None
        val state = agent.state.lowercase()
        return when {
            state.contains("blocked") || state.contains("paused") -> AgentAction.Resume
            state.contains("fail") || state.contains("error") -> AgentAction.Retry
            else -> AgentAction.Steer
        }
    }

    /**
     * One visible primary action shaped by the selected agent's state plus an
     * overflow menu; every command stays reachable and the control map (the
     * test/dispatch hook) is unchanged.
     */
    private fun syncAgentControls() {
        val action = selectedAgentAction()
        primaryAction.text = when (action) {
            AgentAction.Resume -> "Resume"
            AgentAction.Retry -> "Retry"
            AgentAction.Steer, AgentAction.None -> "Steer"
        }
        primaryAction.isEnabled = action != AgentAction.None
        val hasAgent = agentsCombo.selectedItem is NativeAgent
        for (i in 0 until moreMenu.componentCount) {
            val component = moreMenu.getComponent(i)
            if (component is JMenuItem) component.isEnabled = hasAgent
        }
    }

    /**
     * The plan-derived role/title of one agent: the owned step kind and goal
     * when the plan knows it, else the agent's own goal, else its id.
     */
    internal fun agentTitle(agent: NativeAgent): String {
        val role = roles[agent.agentId]
        if (!role.isNullOrBlank()) {
            val goal = bound(agent.goal, 60)
            return if (goal.isBlank()) role else "$role · $goal"
        }
        val goal = bound(agent.goal, 72)
        return if (goal.isBlank()) agent.agentId else goal
    }

    /** The second line: state plus what the agent is doing or waiting on. */
    internal fun agentAction(agent: NativeAgent): String {
        val blocker = agent.blocker
        val progress = agent.progress
        val result = agent.result
        val inFlight = progress?.inFlightOp
        val summary = result?.summary
        val detail = when {
            blocker != null && blocker.reason.isNotBlank() -> bound(blocker.reason, 72)
            inFlight != null -> "running " + bound(inFlight, 60)
            summary != null -> bound(summary, 72)
            else -> ""
        }
        return if (detail.isBlank()) agent.state else agent.state + " · " + detail
    }

    /** One bounded human roster row (renderer + tooltip + smoke share it). */
    internal fun rowText(agent: NativeAgent): String =
        agentTitle(agent) + " — " + agentAction(agent)

    /** The advanced exact row: identities, model, budget, ownership. */
    private fun advancedLine(agent: NativeAgent): String {
        val sb = StringBuilder()
        sb.append(agent.agentId).append(" [").append(agent.kind).append("] ")
            .append(agent.state).append(" ownership=").append(agent.ownership)
            .append(" model=").append(agent.model ?: "-")
            .append(" budget=").append(agent.budget ?: "-")
        spends[agent.agentId]?.let { sb.append(" cost=").append(currency(it)) }
        if (agent.progress?.stalled == true) sb.append(" stalled")
        return sb.toString()
    }

    fun setListener(value: Listener?) {
        listener = value
    }

    /**
     * Replaces the roster and the advanced rows with the served agent list;
     * a still-served selection is preserved, otherwise the combo falls back
     * to the first row (DefaultComboBoxModel's own behavior). [model] feeds
     * the plan-derived role column when the plan knows the agent's step.
     */
    fun update(agents: List<NativeAgent>, model: TaskTreeModel? = null) {
        val roleMap = LinkedHashMap<String, String>()
        val spendMap = LinkedHashMap<String, java.math.BigInteger>()
        if (model != null) {
            for (child in model.children) {
                val kind = child.itemKind?.takeIf { it.isNotBlank() }
                if (kind != null) roleMap[child.childId] = kind
                if (child.spentCostMicro != null) spendMap[child.childId] = child.spentCostMicro
            }
        }
        roles = roleMap
        spends = spendMap
        val selectedId = (agentsCombo.selectedItem as? NativeAgent)?.agentId
        agentsModel.removeAllElements()
        for (agent in agents) agentsModel.addElement(agent)
        if (selectedId != null) selectAgent(selectedId)
        agentsArea.text = if (agents.isEmpty()) {
            "No agents yet. Rows appear here while a run works."
        } else {
            val sb = StringBuilder()
            for (agent in agents) {
                sb.append(advancedLine(agent)).append('\n')
            }
            sb.toString()
        }
        syncAgentControls()
    }

    /** The rendered human roster (smoke observable, no display needed). */
    fun rosterText(): String {
        val sb = StringBuilder()
        for (i in 0 until agentsModel.size) {
            sb.append(rowText(agentsModel.getElementAt(i))).append('\n')
        }
        return sb.toString()
    }

    fun advancedText(): String = agentsArea.text

    /** Selects one agent in the combo (the dialog-path smoke hook). */
    internal fun selectAgent(agentId: String): Boolean {
        for (i in 0 until agentsModel.size) {
            if (agentsModel.getElementAt(i).agentId == agentId) {
                agentsCombo.selectedIndex = i
                return true
            }
        }
        return false
    }

    /** Clicks one agent-control button on the EDT (the dialog-path smoke hook). */
    internal fun clickControl(label: String): Boolean {
        val button = agentControlButtons[label] ?: return false
        if (SwingUtilities.isEventDispatchThread()) {
            button.doClick(0)
        } else {
            SwingUtilities.invokeAndWait { button.doClick(0) }
        }
        return true
    }
}
