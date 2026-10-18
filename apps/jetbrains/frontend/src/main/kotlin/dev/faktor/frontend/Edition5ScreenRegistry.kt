// The 32 Edition 5 screens in their JetBrains rendering form. Settings-cluster
// screens reuse the one configuration center; runtime screens use the native
// Swing surfaces. The registry is the single list the visual matrix and the
// control-exercise suite walk, so a screen cannot exist in one harness only.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.NativeModelInfo
import java.awt.Component
import java.awt.IllegalComponentStateException
import javax.swing.JComponent
import javax.swing.JPanel

internal data class Edition5ScreenContext(
    val clientProvider: () -> NativeClient? = { FaktorConnectionRegistry.client() },
    val sessionId: String? = null,
    val taskId: String? = null,
    val catalog: List<NativeModelInfo> = emptyList(),
    val approval: dev.faktor.shared.NativePermissionEntry? = null,
    val onNotice: (String) -> Unit = {}
)

internal object Edition5ScreenRegistry {

    val screenIds: List<String> = Edition5Catalog.screens.map { it.id }

    fun build(screenId: String, context: Edition5ScreenContext = Edition5ScreenContext()): JComponent {
        val screen = Edition5Catalog.screen(screenId)
        return when (screenId) {
            "S01" -> cockpit(context)
            "S02" -> Edition5ModelPickerPanel(context.catalog)
            "S03", "S04" -> settings(context, "Providers, credentials and local inference")
            "S05" -> settings(context, "Reasoning and generation")
            "S06", "S07" -> settings(context, "Agent presets and delegation")
            "S08" -> withClient(Edition5ContextPanel(context.sessionId), context)
            "S09" -> Edition5MentionsPanel()
            "S10" -> withClient(Edition5CompactionPanel(context.sessionId), context)
            "S11" -> settings(context, "Instructions, rules and prompt composition")
            "S12" -> settings(context, "Compaction, memory and efficiency")
            "S13" -> withClient(Edition5PermissionPolicyPanel(), context)
            "S14" -> Edition5ApprovalDock(context.approval)
            "S15", "S16" -> withClient(Edition5McpPanel(), context)
            "S17", "S18" -> withClient(Edition5SkillsHooksPanel(), context)
            "S19" -> Edition5TerminalSurface()
            "S20" -> withClient(Edition5BrowserPanel(), context)
            "S21" -> settings(context, "Budgets, retries and reliability")
            "S22" -> liveRun(context)
            "S23" -> Edition5AgentsSurface()
            "S24" -> Edition5ChangesSurface()
            "S25" -> withClient(Edition5CheckpointPanel(context.sessionId), context)
            "S26" -> Edition5HistorySurface(context)
            "S27" -> Edition5UsageSurface()
            "S28" -> settings(context, "Compaction, memory and efficiency")
            "S29" -> settings(context, null)
            "S30" -> Edition5DiagnosticsSurface()
            "S31" -> withClient(Edition5BindingPanel(), context)
            "S32" -> withClient(Edition5FinishProofPanel(context.taskId), context)
            else -> error("unknown Edition 5 screen $screenId")
        }.also { component ->
            component.name = "edition5-${screen.id.lowercase()}"
            component.a11y(
                "${screen.id} ${screen.title}",
                "Edition 5 ${screen.cluster.name.lowercase()} screen with ${screen.controls.size} controls."
            )
            installEdition5Accessibility(component, screen.id)
        }
    }

    fun all(context: Edition5ScreenContext = Edition5ScreenContext()): List<Pair<String, JComponent>> =
        screenIds.map { it to build(it, context) }

    /**
     * Fill any missing accessible name on an interactive control. Existing
     * panels already carry meaningful names; this pass covers the generic
     * editor/list widgets without changing their role or state.
     */
    private fun installEdition5Accessibility(component: Component, screenId: String) {
        fun walk(node: Component) {
            val context = node.accessibleContext
            if (context != null && context.accessibleName.isNullOrBlank()) {
                val name = when (node) {
                    is javax.swing.AbstractButton ->
                        (node as? javax.swing.JButton)?.text?.takeIf { it.isNotBlank() }
                            ?: "Action"
                    is javax.swing.JTextField -> node.toolTipText?.takeIf { it.isNotBlank() } ?: "Text field"
                    is javax.swing.JTextArea -> node.toolTipText?.takeIf { it.isNotBlank() } ?: "Text area"
                    is javax.swing.JComboBox<*> -> "Choice"
                    is javax.swing.JList<*> -> "List"
                    else -> null
                }
                if (name != null) {
                    try {
                        context.accessibleName = "$name ($screenId)"
                    } catch (ignored: IllegalComponentStateException) {
                        // A LAF-internal child may refuse a name; the owning
                        // control carries the accessible contract.
                    }
                }
            }
            if (node is java.awt.Container) {
                for (child in node.components) walk(child)
            }
        }
        walk(component)
    }

    private fun settings(context: Edition5ScreenContext, category: String?): JComponent {
        val source: Edition5ConfigSource =
            context.clientProvider()?.let { NativeEdition5ConfigSource(it) }
                ?: Edition5ConfigUnavailable(
                    "contract-required: daemon not connected; the effective snapshot needs " +
                        "GET ${Edition5Routes.CONFIG_EFFECTIVE}"
                )
        val general = object : Edition5SettingsPanel.GeneralSettingsSurface {
            override val component: JComponent = JPanel().apply {
                isOpaque = false
                val body = pageColumn(gap = Spacing.XS, padding = 0)
                body.add(wrappedMutedLabel(
                    "Application scope. The daemon binary path, run defaults and optional control-plane " +
                        "coordinates are host-local; all other categories are served by the daemon and " +
                        "become effective only after an acknowledged atomic apply."
                ))
                add(body)
            }
            override fun generalState(): String =
                "Application scope / daemon where allowed. Revision is read from the daemon."
            override fun applyGeneral(): String? = null
        }
        val panel = Edition5SettingsPanel(
            Edition5SettingsController(source),
            general,
            initialCategory = category
        )
        return panel.component()
    }

    private fun <T : JComponent> withClient(panel: T, context: Edition5ScreenContext): T {
        if (panel is Edition5AsyncPanel) panel.attachClient(context.clientProvider())
        return panel
    }

    private fun cockpit(context: Edition5ScreenContext): JComponent =
        Edition5WorkCockpit(catalog = context.catalog)

    private fun liveRun(context: Edition5ScreenContext): JComponent {
        val panel = Edition5WorkCockpit(catalog = context.catalog)
        return panel
    }

    /** The existing terminal surface with its honest empty state. */
    private fun Edition5TerminalSurface(): JComponent =
        TerminalPanel().apply {
            setUnavailable("No owned process selected. Terminal, run stop and session abort are separate actions.")
        }

    private fun Edition5AgentsSurface(): JComponent =
        AgentsPanel().apply {
            setListener(object : AgentsPanel.Listener {
                override fun onRefresh() {}
            })
            update(emptyList())
        }

    private fun Edition5ChangesSurface(): JComponent =
        ChangesPanel().apply {
            setListener(object : ChangesPanel.Listener {
                override fun onEvidenceSelected(ref: EvidenceRef) {}
                override fun onOpenSourceDiff(model: ChangeReviewModel, rowIndex: Int?) {}
                override fun onOpenClaim(criterionKey: String) {}
                override fun onReturnToRun() {}
                override fun onRecheckBasis() {}
            })
            update(ChangeReviewModel(basis = null, rows = emptyList()))
        }

    private fun Edition5HistorySurface(context: Edition5ScreenContext): JComponent =
        HistoryPanel().apply {
            setListener(object : HistoryPanel.Listener {
                override fun onOpenSession(sessionId: String) {}
                override fun onRefresh() {}
            })
            setUnavailable("History service unavailable; this is not an empty list.")
            setTranscriptSearchListener { query ->
                context.onNotice("Find searches durable pages, not only rendered rows: $query")
            }
        }

    private fun Edition5UsageSurface(): JComponent =
        UsagePanel().apply {
            setListener(object : UsagePanel.Listener {
                override fun onNextPage() {}
                override fun onPreviousPage() {}
                override fun onGrantCredits() {}
            })
            setUnavailable("Usage is unavailable until the daemon serves the ledger.")
        }

    private fun Edition5DiagnosticsSurface(): JComponent =
        DiagnosticsPanel().apply {
            setListener(object : DiagnosticsPanel.Listener {
                override fun onRestart() {}
                override fun onReconnect() {}
                override fun onRefresh() {}
                override fun onWaitForReconnect() {}
                override fun onRefreshFromSnapshot() {}
            })
            setUnavailable("Diagnostics are unavailable until the daemon is ready.")
        }
}
