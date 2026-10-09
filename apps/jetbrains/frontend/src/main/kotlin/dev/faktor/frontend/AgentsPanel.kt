// The background-agents plane: the session's agent roster with its live
// ownership/model/budget readout and the operator controls (pause, resume,
// cancel, retry, steer, model, token budget, cost budget). Pure presentation:
// the control callbacks are delivered to a Listener; the input dialogs and
// every native call stay in FaktorChatPanel, so the panel is display-testable
// and the same component renders in the IDE tool window and the host matrix.
package dev.faktor.frontend

import dev.faktor.shared.NativeAgent
import java.awt.BorderLayout
import java.awt.GridLayout
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

    private lateinit var primaryAction: JButton

    private lateinit var moreMenu: JPopupMenu

    private var listener: Listener? = null

    init {
        val refreshAgents = secondaryButton("Refresh agents")
        refreshAgents.addActionListener { listener?.onRefresh() }
        // The combo renders one bounded identity row, never the raw DTO
        // toString (which clipped at "NativeAgent(agentId=self-1, kind=self,
        // runId=run-9, ses..."); the full row stays available as tooltip.
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
                text = bound(agentLabel(agent), 96)
                toolTipText = agentLabel(agent)
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
        agentsArea.font = uiPanelFont()
        val outputBody = pageColumn(gap = Spacing.XS, padding = 0)
        outputBody.add(insetScroll(agentsArea))
        val page = pageColumn()
        page.add(card("Agent controls", controlsBody))
        page.add(vSpace(Spacing.M))
        page.add(card("Agent detail", outputBody))
        add(pageScroll(page), BorderLayout.CENTER)
        syncAgentControls()
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

    /** One bounded identity row for the combo (shared by renderer + tooltip). */
    private fun agentLabel(agent: NativeAgent): String =
        "${agent.agentId} [${agent.kind}] ${agent.state}"

    fun setListener(value: Listener?) {
        listener = value
    }

    /**
     * Replaces the roster and the detail rows with the served agent list; a
     * still-served selection is preserved, otherwise the combo falls back to
     * the first row (DefaultComboBoxModel's own behavior).
     */
    fun update(agents: List<NativeAgent>) {
        val selectedId = (agentsCombo.selectedItem as? NativeAgent)?.agentId
        agentsModel.removeAllElements()
        for (agent in agents) agentsModel.addElement(agent)
        if (selectedId != null) selectAgent(selectedId)
        agentsArea.text = if (agents.isEmpty()) {
            "No background agents yet. Children appear here while a task runs."
        } else {
            val sb = StringBuilder()
            for (agent in agents) {
                sb.append(agent.agentId).append(" [").append(agent.kind).append("] ")
                    .append(agent.state).append(" ownership=").append(agent.ownership)
                    .append(" model=").append(agent.model ?: "-")
                    .append(" budget=").append(agent.budget ?: "-")
                    .append('\n')
            }
            sb.toString()
        }
        syncAgentControls()
    }

    /** The agent-control dispatch runs only when a row is selected. */
    private fun agentButton(label: String, control: (NativeAgent) -> Unit): JButton {
        val button = secondaryButton(label)
        agentControlButtons[label] = button
        button.addActionListener {
            val agent = agentsCombo.selectedItem as? NativeAgent
            if (agent == null) {
                listener?.onNotice("select an agent first")
                return@addActionListener
            }
            control(agent)
        }
        return button
    }

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
