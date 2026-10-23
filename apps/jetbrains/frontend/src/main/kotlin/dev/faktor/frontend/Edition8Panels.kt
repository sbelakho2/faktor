// Revision 8 JetBrains surfaces: one registry-driven screen renderer plus the
// Work / Inspect / History shell. The renderer is deliberately a thin host
// adapter over Edition8Daemon: fields stage one native configuration patch,
// every probe calls a real named route, and the status line only says
// "Applied"/"Read" after the daemon acknowledged the exact operation.
//
// Keyboard/focus: every interactive component is focusable, carries an
// accessible name and description, and the screen header names the current
// status in words (never color alone). Escape returns focus to the invoker
// through the host navigation callback.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.NativePermissionEntry
import dev.faktor.shared.view
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.Component
import java.awt.Container
import java.awt.Dimension
import java.awt.FlowLayout
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Insets
import java.awt.event.FocusAdapter
import java.awt.event.FocusEvent
import java.awt.event.KeyEvent
import java.util.LinkedHashMap
import javax.swing.AbstractButton
import javax.swing.BorderFactory
import javax.swing.Box
import javax.swing.BoxLayout
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JDialog
import javax.swing.JLabel
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JSplitPane
import javax.swing.JTabbedPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.KeyStroke
import javax.swing.SwingUtilities
import javax.swing.WindowConstants
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

/** The host callbacks the real tool window supplies; tests supply fixtures. */
internal class Edition8Host(
    val client: () -> NativeClient? = { FaktorConnectionRegistry.client() },
    val sessionId: () -> String? = { null },
    val taskId: () -> String? = { null },
    val vault: SecretVault? = null,
    val settings: FaktorSettingsController? = null,
    val startService: (() -> String)? = null,
    val restartService: (() -> String)? = null,
    val confirmTrust: ((String) -> String)? = null,
    val startTask: ((String, Map<String, String>) -> String)? = null,
    val stopScope: ((String) -> String)? = null,
    val replyApproval: ((String, String, String) -> String)? = null,
    /**
     * Open the REVIEW surface that owns the per-request Allow/Deny controls.
     * This callback can only navigate: it has no permission reply and can
     * therefore never approve anything on its own.
     */
    val openApprovals: (() -> String)? = null,
    val openDiff: ((String) -> String)? = null,
    val openTerminal: (() -> String)? = null,
    val inlineCodeAction: (() -> String)? = null,
    val hostAction: ((String) -> String)? = null,
    val navigate: ((String) -> Unit)? = null,
    val notice: (String) -> Unit = {},
    val credentials: ControlPlaneCredentialStore? = null
) {
    val work = Edition8Work()

    /**
     * The browser shell installs an in-dialog navigator and a real Back
     * action. Keeping these as overrides means a screen never opens a second
     * modal copy of itself (the old nested-dialog dead end) and Back always
     * returns to the surface that invoked it.
     */
    internal var navigator: ((String) -> Unit)? = null
    internal var backHandler: (() -> Unit)? = null

    fun daemon(): Edition8Daemon = Edition8Daemon(client, sessionId, taskId, work)

    fun navigateTo(screenId: String) {
        (navigator ?: navigate)?.invoke(screenId)
    }

    fun canGoBack(): Boolean = backHandler != null || navigate != null

    fun goBack(): Boolean {
        backHandler?.invoke()?.let { return true }
        if (navigate != null) {
            navigate.invoke("S01")
            return true
        }
        return false
    }

    fun shutdown() = work.close()
}

/**
 * The IDE-editor host seam for the controls whose effect belongs to the real
 * editor (inline code action, current-selection capture, slash-command insert,
 * diagnostics tool window). The split-mode kotlinc smoke compiles without the
 * IntelliJ platform jars, so `FaktorChatPanel` depends on this interface only;
 * `IntelliJHostEditor` supplies the platform implementation in the plugin
 * build. A null host keeps every callback typed `contract-required` instead
 * of pretending an editor effect happened.
 */
interface Edition8HostEditor {
    fun inlineCodeAction(): String
    /** The capture line to store, or null when no editor selection exists. */
    fun captureSelection(): String?
    fun insertSlashCommand(): String
    fun openProblems(): String
}

/** Controls whose effect belongs to the host editor (no daemon write exists). */
internal val HOST_CONTROLS: Set<String> = setOf(
    "M-03", "C-04", "Q-11", "S-05", "X-10",
    "D-08", "D-09", "D-10", "D-11", "D-14"
)

/** Screens whose served proof receipts are rendered below the controls. */
internal val PROOF_SCREENS: Set<String> = setOf("S45", "S46", "S48")

/**
 * Screens whose primary action is the one atomic settings Apply. Keeping the
 * apply button as the only primary removes the old pair of competing
 * primaries ("Save Connection" plus "Apply settings"), and the button is
 * disabled until a value actually changes.
 */
internal val APPLY_PRIMARY_SCREENS: Set<String> = setOf(
    "S03", "S04", "S05", "S12", "S13", "S16", "S18", "S21",
    "S28", "S29", "S57", "S60", "S64"
)

/** User-facing Rev8 copy; control IDs and screen IDs stay unchanged. */
internal object Edition8Copy {

    fun primaryLabel(screen: Edition8Screen): String = when (screen.id) {
        "S14" -> "Review Approval"
        "S29", "S64" -> "Apply Settings"
        "S54" -> "Refresh"
        "S02" -> "Open Tuning"
        "S06" -> "Open Agent"
        "S09" -> "Open Context"
        "S57" -> "Keep Fail-Closed"
        "S55", "S68" -> "Open Sign-In Settings"
        "S60" -> "Save Resolved Settings"
        "S10" -> "Review Context"
        "S11" -> "Review Rules"
        "S15" -> "Add MCP Server"
        "S16" -> "Save MCP Server"
        "S17" -> "Review Skills"
        "S23", "S50" -> "Review Agents"
        "S25" -> "Review Checkpoints"
        "S26", "S71", "S72" -> "Review Conversations"
        "S33" -> "Review Options"
        "S39" -> "Review Task"
        "S40" -> "Review Question"
        "S47", "S48" -> "Review Proof"
        "S52" -> "Review Board"
        "S59" -> "Review Queue"
        "S66" -> "Delete Memory"
        "S69" -> "Open Instructions"
        "S03" -> "Save Connection"
        "S04" -> "Save Local Inference"
        "S05" -> "Apply Tuning"
        "S07" -> "Review Agent"
        "S08", "S62" -> "Open Sources"
        "S12" -> "Save Memory"
        "S13" -> "Apply Policy"
        "S18" -> "Save Hook"
        "S21" -> "Apply Defaults"
        "S28" -> "Apply"
        "S35" -> "Confirm Path"
        "S41" -> "Review Missing Evidence"
        "S45" -> "Open AC-03 Details"
        "S46" -> "Open Full Output"
        "S70" -> "Stop Run"
        else -> screen.primaryAction
            .substringBefore(" · JetBrains:")
            .removePrefix("VS Code: ")
            .replace(" (composer)", "")
            .trim()
    }

    fun applyLabel(screen: Edition8Screen): String = when (screen.id) {
        "S03" -> "Save Connection"
        "S04" -> "Save Local Inference"
        "S05" -> "Apply Tuning"
        "S12" -> "Save Memory"
        "S13" -> "Apply Policy"
        "S16" -> "Save MCP Server"
        "S18" -> "Save Hook"
        "S21" -> "Apply Defaults"
        "S28" -> "Apply"
        "S29", "S64" -> "Apply Settings"
        "S57" -> "Keep Fail-Closed"
        "S60" -> "Save Resolved Settings"
        else -> "Apply Changes"
    }

    fun hasPrimary(screen: Edition8Screen): Boolean = true

    fun formTitle(screen: Edition8Screen, editable: Boolean): String = when {
        !editable -> "Details"
        screen.group == "Settings and diagnostics" -> "Settings"
        screen.group == "Models and providers" -> "Provider settings"
        screen.id in APPLY_PRIMARY_SCREENS -> "Settings"
        else -> "Options"
    }

    fun summary(screen: Edition8Screen): String = when (screen.id) {
        "S01" -> "One task. Proof stays separate from the chat claim."
        "S03" -> "Connect once; the daemon must acknowledge the pair."
        "S14" -> "Allow once = this request only; deny stops this request."
        "S24" -> "Review before accepting; unverified changes stay visible."
        "S29" -> "Staged locally. Apply commits one atomic patch."
        "S34" -> "Start once; trust, model and isolation are separate."
        "S36" -> "Describe one task; admission captures its bounds."
        "S45" -> "Acceptance, evidence, integration and finish stay separate."
        "S53", "S54" -> "Four cost categories; unavailable is not zero."
        "S58" -> "Narrow host width: one primary, no horizontal scroll."
        "S61" -> "No conversations yet; start the first task."
        "S70" -> "Choose the scope you mean; issued effects cannot be undone."
        else -> when (screen.group) {
            "Setup and start" -> "Start once; trust and isolation are separate facts."
            "Work" -> "One task at a time; a chat claim never becomes proof."
            "Inspect and proof" -> "Acceptance, evidence, integration and finish stay separate."
            "Agents" -> "Bounded delegation; steering applies to the next turn."
            "History" -> "Durable conversations, newest first."
            "Usage" -> "Four cost categories; unavailable is not zero."
            "Models and providers" -> "A model is captured only after the daemon acknowledges it."
            "Context" -> "Usage comes from the durable context manifest."
            "Instructions and memory" -> "Rules and memory are scoped; precedence names the winner."
            "Permissions and tools" -> "Approvals are per request; rules cover future matches."
            "Settings and diagnostics" -> "Staged locally; Apply commits one atomic patch."
            else -> "Revision 8 native surface."
        }
    }
}

internal object Edition8ScreenRegistry {

    val screenIds: List<String> = Edition8Registry.screens.map { it.id }

    fun build(screenId: String, host: Edition8Host): JComponent {
        val screen = Edition8Registry.screen(screenId)
        val panel = Edition8ScreenPanel(screen, host)
        panel.name = "edition8-${screen.id.lowercase()}"
        panel.a11y(
            "${screen.id} ${screen.title}",
            "Revision 8 ${screen.group.lowercase()} screen. Primary action " +
                "${screen.primaryAction}. ${screen.controls.size} controls."
        )
        installScrollButtonAccessibility(panel)
        return panel
    }

    fun all(host: Edition8Host): List<Pair<String, JComponent>> =
        screenIds.map { it to build(it, host) }

    fun shell(host: Edition8Host): JComponent = Edition8ShellPanel(host)
}

/** One editable/staged row: label, control, apply timing, native state. */
internal class Edition8ControlRow(
    val control: Edition8Control,
    private val panel: Edition8ScreenPanel
) : JPanel() {

    val field: JTextField? =
        if (control.configKeys.isNotEmpty() || control.id == "X-06") JTextField(24) else null
    val toggle: JCheckBox? = if (isToggle(control) && control.configKeys.isNotEmpty()) {
        // FaktorCheckBox carries the 24px hit-target floor in its actual
        // preferred bounds (the bare platform box is only 23px at 100%).
        FaktorCheckBox("Enabled")
    } else {
        null
    }
    val probe: JButton = secondaryButton(if (control.id in HOST_CONTROLS) "Open" else "Read")
    private val stateLabel = WrappedLabel(Edition8Stated.initial(control))
    private var stateText = Edition8Stated.initial(control)
    private val configKey: String? = control.configKeys.firstOrNull()

    init {
        isOpaque = false
        layout = BoxLayout(this, BoxLayout.Y_AXIS)
        border = BorderFactory.createEmptyBorder(0, 0, Spacing.M, 0)

        val label = WrappedLabel("${control.id} · ${control.name}").apply {
            font = sectionTitleFont()
            foreground = textForeground()
            alignmentX = Component.LEFT_ALIGNMENT
        }
        val detail = wrappedMutedLabel(
            bound(
                control.control + if (control.configKeys.isNotEmpty()) " · saved with Apply" else "",
                220
            )
        )
        val header = JPanel(BorderLayout(Spacing.S, 0)).apply {
            isOpaque = false
            alignmentX = Component.LEFT_ALIGNMENT
            add(label, BorderLayout.CENTER)
            add(probe, BorderLayout.EAST)
        }
        add(header)
        add(detail)

        if (field != null) {
            field.name = "edition8-field-${control.id}"
            field.a11y(
                "${control.id} ${control.name}",
                "Value for ${control.control}. ${control.applyTiming}."
            )
            field.text = initialFieldValue()
            field.toolTipText = if (field.text.isNullOrBlank()) {
                "No value is loaded yet. Read the screen or enter a value; Apply commits one patch."
            } else {
                control.defaultValue
            }
            if (configKey != null && panel.daemon.draft.isLocked(configKey)) {
                field.isEnabled = false
                field.toolTipText = "Set by managed policy; read-only"
            }
            field.alignmentX = Component.LEFT_ALIGNMENT
            add(vSpace(Spacing.XS))
            add(field)
        }
        if (toggle != null) {
            toggle.alignmentX = Component.LEFT_ALIGNMENT
            add(vSpace(Spacing.XS))
            add(toggle)
        }

        stateLabel.font = uiPanelFont()
        stateLabel.foreground = mutedForeground()
        stateLabel.alignmentX = Component.LEFT_ALIGNMENT
        stateLabel.a11y(
            "State for ${control.id} ${control.name}",
            "The last acknowledged state of this control."
        )
        add(vSpace(Spacing.XS))
        add(stateLabel)

        probe.name = "edition8-activate-${control.id}"
        probe.a11y(
            "${control.id} ${control.name}: ${control.control}",
            "Reads or applies ${control.name}. ${control.behavior}"
        )
        probe.addActionListener { panel.activate(this) }
        // A field/toggle is the control; a separate per-row Read button beside
        // every editable value was a duplicate action. Keep the registered
        // handler for the full control contract, but expose one action per
        // read-only row and one Apply for staged edits.
        probe.isVisible = field == null && toggle == null

        val fieldListener = object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent?) = stage()
            override fun removeUpdate(e: DocumentEvent?) = stage()
            override fun changedUpdate(e: DocumentEvent?) = stage()
            private fun stage() = panel.stage(this@Edition8ControlRow)
        }
        field?.document?.addDocumentListener(fieldListener)
        toggle?.addActionListener { panel.stage(this) }
        toggle?.a11y(
            "${control.id} ${control.name}",
            "Toggle ${control.name}. ${control.behavior}"
        )
        a11y("${control.id} ${control.name}", control.stateContract)
    }

    fun value(): String = toggle?.let { if (it.isSelected) "true" else "false" }
        ?: field?.text ?: ""

    /** A descriptive catalog default is not a served value; leave it empty. */
    private fun initialFieldValue(): String {
        if (control.id == "X-06") return "https://example.com/"
        val value = control.defaultValue.trim()
        if (value.isEmpty() || value.equals("none", true)) return ""
        if (value.equals("true", true) || value.equals("false", true)) return value
        if (value.any { it.isDigit() } || value.startsWith("/") || value.startsWith("http") ||
            value.startsWith("~") || value.contains("://")
        ) return value
        return ""
    }

    fun setState(text: String, state: SemanticState) {
        stateText = text
        stateLabel.fullText = text
        stateLabel.foreground = semanticForeground(state)
        stateLabel.toolTipText = text
        stateLabel.accessibleContext?.accessibleDescription = text
    }

    fun stateText(): String = stateText

    private fun isToggle(control: Edition8Control): Boolean {
        val text = (control.control + " " + control.name + " " + control.behavior).lowercase()
        return text.contains("toggle") || text.contains("checkbox") || text.contains("switch") ||
            control.defaultValue.equals("true", true) || control.defaultValue.equals("false", true)
    }
}

internal object Edition8Stated {
    fun initial(control: Edition8Control): String =
        if (control.configKeys.isEmpty()) "Not read" else "Not saved"

    fun sourceOf(control: Edition8Control): String =
        control.endpoints.firstOrNull { it.state == "exists" }?.actual
            ?: control.endpoints.firstOrNull()?.declared
            ?: "no native route named"
}

/** One real screen. All I/O is delegated to [Edition8Daemon] off the EDT. */
private data class PermissionDisplayRow(
    val id: String,
    val session: String,
    val capability: String,
    val detail: String
)

internal class Edition8ScreenPanel(
    val screen: Edition8Screen,
    val host: Edition8Host
) : JPanel(BorderLayout()) {

    val daemon: Edition8Daemon = host.daemon()
    val rows: List<Edition8ControlRow>
    private val applyIsPrimary: Boolean = screen.id in APPLY_PRIMARY_SCREENS
    private val keyRows = ArrayList<Edition8ControlRow>()
    private val extraRows = ArrayList<Edition8ControlRow>()
    private val advancedToggle: JButton = secondaryButton("Show more options")
    private val advancedPanel = JPanel()
    private val status: JLabel = statusChip("Not read", SemanticState.DIM)
    private val detail = JTextArea(4, 40).apply {
        isEditable = false
        lineWrap = true
        wrapStyleWord = true
        font = monospacePanelFont()
        name = "edition8-${screen.id.lowercase()}-detail"
        a11y("${screen.id} native snapshot", "Bounded daemon response for ${screen.title}.")
    }
    private val detailScroll = insetScroll(detail)
    private val detailPanel = JPanel(BorderLayout())
    private val detailToggle = secondaryButton("Show last response")
    private val receipts = pageColumn(gap = Spacing.XS, padding = 0)
    private val primary: JButton = primaryButton(
        if (screen.id in APPLY_PRIMARY_SCREENS) Edition8Copy.applyLabel(screen) else Edition8Copy.primaryLabel(screen)
    )
    private val apply: JButton = if (applyIsPrimary) {
        primaryButton(Edition8Copy.applyLabel(screen))
    } else {
        secondaryButton("Apply changes")
    }
    private val retry: JButton = secondaryButton("Retry")
    private val back: JButton = secondaryButton("Back")

    /** Spec S54 navigation: Account Settings… opens S55 without inventing a control. */
    private val accountSettings: JButton = secondaryButton("Account Settings…")
    private val hint: JLabel = wrappedMutedLabel("")
    private val statusDetail: JLabel = wrappedMutedLabel("")
    private var closed = false
    private var loadingValues = false
    private var fullStatus = "Not read"

    init {
        rows = Edition8Registry.controlsForScreen(screen.id).map { Edition8ControlRow(it, this) }
        chooseVisibleRows()
        buildScaffolding()
        add(pageScroll(buildBody()), BorderLayout.CENTER)
        add(buildFooter(), BorderLayout.SOUTH)
        wireActions()
        installEscape()
        loadConfigIfNeeded()
        loadProofReceipts()
        focusFirstAction()
    }

    private fun chooseVisibleRows() {
        if (rows.size <= 4) {
            keyRows.addAll(rows)
            return
        }
        val editable = rows.filter { it.field != null || it.toggle != null }
        for (row in editable.take(3)) keyRows.add(row)
        for (row in rows) {
            if (keyRows.size >= 3) break
            if (row !in keyRows) keyRows.add(row)
        }
        for (row in rows) if (row !in keyRows) extraRows.add(row)
    }

    private fun buildScaffolding() {
        advancedPanel.isOpaque = false
        advancedPanel.layout = BoxLayout(advancedPanel, BoxLayout.Y_AXIS)
        advancedPanel.isVisible = false
        for (row in extraRows) advancedPanel.add(row)
        detailPanel.isOpaque = false
        detailPanel.add(detailScroll, BorderLayout.CENTER)
        detailPanel.isVisible = false
        detailScroll.preferredSize = Dimension(10, 150)
        hint.isVisible = false
        retry.isVisible = false
        detailToggle.isVisible = false
    }

    private fun wireActions() {
        primary.name = "edition8-primary-${screen.id}"
        primary.a11y(
            "${screen.id} ${primary.text}",
            "Primary action for ${screen.title}. ${screen.states}"
        )
        primary.isVisible = !applyIsPrimary && Edition8Copy.hasPrimary(screen)
        primary.addActionListener { runPrimary() }
        apply.name = "edition8-apply-${screen.id}"
        apply.addActionListener { applySettings() }
        apply.isVisible = false
        apply.isEnabled = false
        refreshApply()
        accountSettings.name = "edition8-account-settings-${screen.id}"
        accountSettings.isVisible = screen.id == "S54"
        accountSettings.a11y(
            "Account Settings for ${screen.title}",
            "Opens the optional control-plane account screen (S55)."
        )
        accountSettings.addActionListener { navigateOr("S55") }
        back.a11y("Back from ${screen.title}", "Returns to the invoking surface with the draft intact.")
        back.addActionListener {
            if (!host.goBack()) {
                setStatus("Back is available from the host window", SemanticState.DIM)
            }
        }
        retry.name = "edition8-retry-${screen.id}"
        retry.a11y("Retry loading ${screen.title}", "Retries the same read; no value is invented.")
        retry.addActionListener {
            retry.isVisible = false
            for (row in rows) {
                if (row.field != null || row.toggle != null) row.setState("Not saved", SemanticState.DIM)
            }
            loadConfigIfNeeded()
        }
        advancedToggle.name = "edition8-more-${screen.id}"
        advancedToggle.text = "Show ${extraRows.size} more options"
        advancedToggle.a11y(
            "Show more options for ${screen.title}",
            "Reveals the remaining registry controls without leaving this screen."
        )
        advancedToggle.addActionListener {
            advancedPanel.isVisible = !advancedPanel.isVisible
            advancedToggle.text = if (advancedPanel.isVisible) {
                "Hide extra options"
            } else {
                "Show ${extraRows.size} more options"
            }
            revalidate()
            repaint()
        }
        detailToggle.name = "edition8-detail-toggle-${screen.id}"
        detailToggle.a11y("Show last response for ${screen.title}", "Shows the bounded daemon response.")
        detailToggle.addActionListener {
            detailPanel.isVisible = !detailPanel.isVisible
            detailToggle.text = if (detailPanel.isVisible) "Hide last response" else "Show last response"
            revalidate()
            repaint()
        }
    }

    private fun buildHeader(): JComponent {
        val eyebrow = mutedLabel("${screen.id} · ${screen.group}")
        val title = WrappedLabel(screen.title).apply {
            font = uiPanelFont().deriveFont(java.awt.Font.BOLD, uiPanelFont().size2D + 5f)
            foreground = textForeground()
            name = "edition8-title-${screen.id}"
            a11y(screen.title, "${screen.group} screen.")
        }
        status.name = "edition8-status-${screen.id}"
        status.a11y("Status: ${status.text}", "One status line for ${screen.title}.")
        statusDetail.name = "edition8-status-detail-${screen.id}"
        statusDetail.alignmentX = Component.LEFT_ALIGNMENT
        val summary = wrappedMutedLabel(Edition8Copy.summary(screen))
        summary.name = "edition8-summary-${screen.id}"
        summary.alignmentX = Component.LEFT_ALIGNMENT
        eyebrow.alignmentX = Component.LEFT_ALIGNMENT
        status.alignmentX = Component.LEFT_ALIGNMENT
        val column = JPanel().apply {
            isOpaque = false
            name = "edition8-header-${screen.id}"
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            border = BorderFactory.createEmptyBorder(Spacing.L, Spacing.L, Spacing.S, Spacing.L)
            alignmentX = Component.LEFT_ALIGNMENT
        }
        column.add(eyebrow)
        column.add(title)
        column.add(vSpace(Spacing.XS))
        column.add(status)
        column.add(statusDetail)
        column.add(vSpace(Spacing.XS))
        column.add(summary)
        column.alignmentX = Component.LEFT_ALIGNMENT
        column.maximumSize = Dimension(Integer.MAX_VALUE, Integer.MAX_VALUE)
        return column
    }

    private fun buildBody(): JComponent {
        // The header scrolls with the page. At 200% text a fixed header can
        // consume the whole viewport; keeping it in the page leaves the
        // keyboard-reachable primary fixed and lets every heading be read.
        val body = pageColumn(gap = 0, padding = 0)
        body.add(buildHeader())
        body.add(buildHero())
        body.add(buildForm())
        return body
    }

    private fun buildHero(): JComponent {
        val hero = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            border = BorderFactory.createEmptyBorder(0, Spacing.L, Spacing.S, Spacing.L)
        }
        var hasContent = false
        fun add(component: Component) {
            hero.add(component)
            hasContent = true
        }
        when (screen.id) {
            "S34", "S35", "S57" -> {
                add(wrapRow(
                    checklist("Service executable", "Unselected"),
                    checklist("Model", "Not connected"),
                    checklist("Shell isolation", "Fail-closed")
                ))
            }
            "S01", "S36", "S38", "S39", "S58", "S59" -> {
                val goal = JTextArea(3, 40).apply {
                    lineWrap = true
                    wrapStyleWord = true
                    name = "edition8-goal"
                    alignmentX = Component.LEFT_ALIGNMENT
                    border = BorderFactory.createCompoundBorder(
                        BorderFactory.createLineBorder(cardBorderColor()),
                        BorderFactory.createEmptyBorder(Spacing.S, Spacing.S, Spacing.S, Spacing.S)
                    )
                    a11y("Task goal", "One task. Cmd+Enter runs; the draft survives admission.")
                    bindCmdEnter { runPrimary() }
                }
                val goalLabel = sectionHeader("Task goal").apply { alignmentX = Component.LEFT_ALIGNMENT }
                add(goalLabel)
                add(goal)
                val admission = wrappedMutedLabel(
                    "A goal, model, effort, authority and caps are captured at admission. " +
                        "A chat claim never fills a proof boundary."
                )
                admission.alignmentX = Component.LEFT_ALIGNMENT
                add(admission)
            }
            "S14" -> {
                add(wrappedMutedLabel(
                    "Allow once = this request only. A remember rule is optional and covers future " +
                        "matching requests. Deny stops this request. Issued commands and network " +
                        "calls cannot be undone."
                ))
            }
            "S70" -> {
                add(wrappedMutedLabel(
                    "Stop run cancels queued work; stopping the response only ends output. Killing " +
                        "the owned process is a separate operation. Issued commands and network calls " +
                        "cannot be undone."
                ))
            }
            "S54" -> {
                add(wrappedMutedLabel("Read-only view. Usage has no editable value on this screen."))
            }
            "S61" -> {
                add(wrappedMutedLabel("No conversations yet. Start a task to create the first durable conversation."))
            }
        }
        for (child in hero.components) {
            (child as? javax.swing.JComponent)?.alignmentX = Component.LEFT_ALIGNMENT
        }
        if (!hasContent) {
            hero.isVisible = false
        }
        return hero
    }

    private fun checklist(name: String, state: String): JComponent =
        JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            add(sectionHeader(name))
            add(mutedLabel(state))
        }

    private fun buildForm(): JComponent {
        val column = pageColumn(gap = Spacing.S, padding = Spacing.L)
        column.add(sectionHeader(Edition8Copy.formTitle(screen, rows.any { it.field != null || it.toggle != null })))
        for (row in keyRows) column.add(row)
        if (extraRows.isNotEmpty()) {
            column.add(vSpace(Spacing.XS))
            column.add(wrapRow(advancedToggle))
            column.add(advancedPanel)
        }
        column.add(vSpace(Spacing.XS))
        column.add(wrapRow(detailToggle))
        column.add(detailPanel)
        column.add(wrapRow(retry))
        if (screen.id in PROOF_SCREENS) {
            column.add(vSpace(Spacing.M))
            column.add(sectionHeader("Served proof receipts"))
            column.add(receipts)
        }
        return column
    }

    private fun buildFooter(): JComponent {
        val actions = wrapRow(primary, apply, accountSettings, back)
        val column = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            border = BorderFactory.createEmptyBorder(Spacing.S, Spacing.L, Spacing.L, Spacing.L)
        }
        column.add(actions)
        column.add(hint)
        return column
    }

    private fun refreshApply() {
        val dirty = daemon.draft.changes().isNotEmpty()
        if (applyIsPrimary) {
            // The apply action IS this surface's primary. Keeping a second
            // visible Apply button was the old competing-primary defect; the
            // legacy apply control stays dispatchable for the control matrix.
            apply.isVisible = false
            apply.isEnabled = dirty
            apply.text = Edition8Copy.applyLabel(screen)
            primary.isVisible = true
            primary.text = Edition8Copy.applyLabel(screen)
            primary.isEnabled = true
            primary.a11y(
                "${screen.id} ${primary.text}",
                "Commits the staged configuration as one atomic patch. Use Apply after editing a value."
            )
            apply.a11y(
                "${screen.id} ${apply.text}",
                "Commits the staged configuration as one atomic patch."
            )
        } else {
            val configurable = rows.any { it.field != null || it.toggle != null }
            apply.isVisible = configurable && dirty
            apply.isEnabled = dirty
            apply.text = "Apply changes"
            apply.a11y(
                "Apply staged ${screen.title} changes",
                "Commits one atomic configuration patch and waits for the daemon acknowledgement."
            )
        }
        hint.text = if (dirty) "Changes are staged locally; Apply commits one validated patch." else ""
        hint.isVisible = dirty
        revalidate()
        repaint()
    }

    private fun setDetail(body: String, max: Int = 4000) {
        detail.text = bound(body, max)
        detailToggle.isVisible = true
        detailToggle.text = "Show last response"
        revalidate()
        repaint()
    }

    private fun focusFirstAction() {
        javax.swing.SwingUtilities.invokeLater {
            val target = rows.firstOrNull { it.field?.isEnabled == true }?.field
                ?: rows.firstOrNull { it.toggle?.isEnabled == true }?.toggle
                ?: primary.takeIf { it.isVisible && it.isEnabled }
                ?: apply.takeIf { it.isVisible && it.isEnabled }
            target?.requestFocusInWindow()
        }
    }

    /** F260/F261: render the SERVED proof receipts, never a fabricated one. */
    private fun loadProofReceipts() {
        if (screen.id !in PROOF_SCREENS) return
        receipts.removeAll()
        if (!daemon.hasTask()) {
            receipts.add(mutedLabel("No task is bound; task proof receipts are task-scoped."))
            receipts.revalidate()
            receipts.repaint()
            return
        }
        receipts.add(mutedLabel("Reading the task proof…"))
        receipts.revalidate()
        receipts.repaint()
        daemon.taskProof { result ->
            if (closed) return@taskProof
            receipts.removeAll()
            when (result) {
                is Edition8Result.Ok -> {
                    val proof = try {
                        JsonCodec.parse(result.body).view("proof")
                    } catch (failure: Throwable) {
                        receipts.add(mutedLabel("Unavailable — malformed proof: ${Edition8Work.describe(failure)}"))
                        receipts.revalidate()
                        receipts.repaint()
                        return@taskProof
                    }
                    var rendered = 0
                    if (screen.id == "S45" || screen.id == "S46") {
                        val criteria = proof.optionalField("criteria")
                            ?.optionalField("items")?.array().orEmpty()
                        for (item in criteria.take(40)) {
                            val key = item.optionalField("criterionKey")?.string()
                                ?: item.optionalField("criterionId")?.string() ?: "criterion"
                            val verdict = item.optionalField("verdict")?.string() ?: "pending"
                            val evidence = item.optionalField("evidence")?.string()
                            if (screen.id == "S46" && evidence == null) continue
                            val text = if (evidence == null) "$key · $verdict" else "$key · $verdict · $evidence"
                            val button = secondaryButton(bound(text, 180))
                            button.name = "edition8-proof-" + key.filter { it.isLetterOrDigit() || it == '-' }.take(40)
                            button.a11y(
                                "Open proof receipt for $key",
                                "Shows the served evidence/provenance for $key; no value is invented."
                            )
                            button.addActionListener {
                                setDetail(item.rawJson(), 4000)
                                setStatus("Receipt — $key · $verdict", SemanticState.POSITIVE)
                                if (screen.id == "S45") host.navigateTo("S46")
                            }
                            receipts.add(button)
                            rendered += 1
                        }
                    }
                    if (screen.id == "S46" || screen.id == "S48") {
                        val completion = proof.optionalField("completion")
                        val steps = completion?.optionalField("records")?.array().orEmpty()
                        for (step in steps.take(40)) {
                            val name = step.optionalField("step")?.string() ?: "step"
                            val status = step.optionalField("status")?.string() ?: "missing"
                            val snapshot = step.optionalField("snapshot")?.string()
                            val button = secondaryButton(bound("$name · $status" + if (snapshot == null) "" else " · $snapshot", 180))
                            button.name = "edition8-step-" + name.filter { it.isLetterOrDigit() || it == '-' }.take(40)
                            button.a11y(
                                "Open completion-step receipt $name",
                                "Shows the durable step row; a missing row stays missing."
                            )
                            button.addActionListener {
                                setDetail(step.rawJson(), 4000)
                                setStatus("Receipt — $name · $status", SemanticState.POSITIVE)
                            }
                            receipts.add(button)
                            rendered += 1
                        }
                    }
                    if (screen.id == "S48") {
                        val publication = proof.optionalField("publication")
                        val commit = publication?.optionalField("commitOid")?.string()
                        val remote = publication?.optionalField("remoteRef")?.string()
                        val local = publication?.optionalField("localRef")?.string()
                        val pr = publication?.optionalField("pullRequest")
                        for ((label, value) in listOf(
                            "Finish receipt" to commit,
                            "Local ref" to local,
                            "SCM target" to remote,
                            "Pull request" to (pr?.optionalField("state")?.string()?.let {
                                (pr.optionalField("id")?.string() ?: "PR") + " · " + it
                            })
                        )) {
                            if (value == null) continue
                            receipts.add(mutedLabel("$label: $value"))
                            rendered += 1
                        }
                    }
                    if (rendered == 0) {
                        receipts.add(mutedLabel("No required check or evidence receipt is served for this task."))
                    }
                }
                is Edition8Result.Failed -> receipts.add(mutedLabel("Unavailable — ${result.reason}"))
                is Edition8Result.Refused -> receipts.add(mutedLabel("Disabled — ${result.reason}"))
            }
            receipts.revalidate()
            receipts.repaint()
        }
    }

    private fun installEscape() {
        val key = KeyStroke.getKeyStroke(KeyEvent.VK_ESCAPE, 0)
        getInputMap(JComponent.WHEN_ANCESTOR_OF_FOCUSED_COMPONENT).put(key, "edition8.back")
        actionMap.put("edition8.back", object : javax.swing.AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) {
                back.doClick()
            }
        })
    }

    private fun loadConfigIfNeeded() {
        if (rows.none { it.field != null || it.toggle != null }) return
        setStatus("Loading settings…", SemanticState.WARNING)
        daemon.loadConfig { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    loadingValues = true
                    try {
                        daemon.draft.load(result.body)
                        for (row in rows) {
                            val key = row.control.configKeys.firstOrNull() ?: continue
                            val value = daemon.draft.value(key)
                            row.field?.text = value
                            row.toggle?.isSelected = value.equals("true", true)
                            row.setState(
                                if (daemon.draft.isLocked(key)) "Locked by organization — read-only"
                                else "Loaded · revision ${daemon.draft.revision}",
                                if (daemon.draft.isLocked(key)) SemanticState.DIM else SemanticState.POSITIVE
                            )
                        }
                        setStatus("Settings read — revision ${daemon.draft.revision}", SemanticState.POSITIVE)
                        retry.isVisible = false
                    } catch (t: Throwable) {
                        setStatus("Unavailable — ${Edition8Work.describe(t)}; Retry reloads the same snapshot", SemanticState.NEGATIVE)
                        retry.isVisible = true
                    } finally {
                        loadingValues = false
                    }
                    refreshApply()
                }
                is Edition8Result.Failed -> {
                    setStatus("Unavailable — ${result.reason}; Retry reloads the same snapshot", SemanticState.NEGATIVE)
                    markConfigUnavailable()
                }
                is Edition8Result.Refused -> {
                    setStatus("Disabled — ${result.reason}", SemanticState.DIM)
                    markConfigUnavailable()
                }
            }
        }
    }

    private fun markConfigUnavailable() {
        for (row in rows) {
            if (row.field != null || row.toggle != null) {
                row.setState("Not loaded — no value was invented", SemanticState.DIM)
            }
        }
        retry.isVisible = true
        refreshApply()
    }

    fun stage(row: Edition8ControlRow) {
        if (loadingValues) return
        val key = row.control.configKeys.firstOrNull() ?: return
        if (daemon.draft.isLocked(key)) {
            row.setState("Locked by organization — change refused", SemanticState.DIM)
            return
        }
        val value = row.value()
        daemon.draft.stage(key, value)
        row.setState(
            if (daemon.draft.changes().containsKey(key)) "Staged — not applied" else "Unchanged · revision ${daemon.draft.revision}",
            if (daemon.draft.changes().containsKey(key)) SemanticState.WARNING else SemanticState.POSITIVE
        )
        setStatus(
            if (daemon.draft.changes().isEmpty()) "No unsaved changes (revision ${daemon.draft.revision})"
            else "Staged ${daemon.draft.changes().size} change(s) — not saved",
            SemanticState.WARNING
        )
        refreshApply()
    }

    fun activate(row: Edition8ControlRow) {
        if (closed) return
        if (row.control.id in HOST_CONTROLS) {
            val action = host.hostAction
            if (action == null) {
                row.setState("Disabled — no host action callback is wired", SemanticState.DIM)
                setStatus("Disabled — this control is host-owned", SemanticState.DIM)
                return
            }
            val result = try {
                action(row.control.id)
            } catch (failure: Throwable) {
                "contract-required: host action failed: ${failure.message ?: failure.javaClass.simpleName}"
            }
            setDetail(result, 4000)
            row.setState("Host — ${row.control.name}", SemanticState.POSITIVE)
            setStatus("Host action — $result", SemanticState.POSITIVE)
            return
        }
        if (row.control.id == "T-12") {
            setStatus("Reading resources and prompts…", SemanticState.WARNING)
            row.setState("Loading — bounded MCP catalog read", SemanticState.WARNING)
            daemon.mcpServers { servers ->
                val id = firstMcpServerId(servers)
                if (id == null) {
                    row.setState("Disabled — no MCP server is configured", SemanticState.DIM)
                    setStatus("Disabled — configure and start an MCP server first", SemanticState.DIM)
                    return@mcpServers
                }
                daemon.mcpCatalogs(id) { result ->
                    when (result) {
                        is Edition8Result.Ok -> {
                            setDetail(result.body, 4000)
                            row.setState("Read — live MCP resources/prompts catalog", SemanticState.POSITIVE)
                            setStatus("Read — ${responseSummary(result.body)}", SemanticState.POSITIVE)
                        }
                        is Edition8Result.Failed -> {
                            row.setState("Failed — ${result.reason}", SemanticState.NEGATIVE)
                            setStatus("Unavailable — ${result.reason}", SemanticState.NEGATIVE)
                        }
                        is Edition8Result.Refused -> {
                            row.setState("Disabled — ${result.reason}", SemanticState.DIM)
                            setStatus("Disabled — ${result.reason}", SemanticState.DIM)
                        }
                    }
                }
            }
            return
        }
        if (row.control.id == "X-08" || row.control.id == "X-09") {
            setStatus("Reading the live browser session…", SemanticState.WARNING)
            row.setState("Loading — bounded browser read", SemanticState.WARNING)
            daemon.browserSessions { sessions ->
                val browserId = firstBrowserSessionId(sessions)
                if (browserId == null) {
                    row.setState("Disabled — no live browser session is attached; connect it first", SemanticState.DIM)
                    setStatus("Disabled — no live browser session; the daemon never fabricates one", SemanticState.DIM)
                    return@browserSessions
                }
                if (row.control.id == "X-08") {
                    daemon.browserScreenshot(browserId) { result ->
                        when (result) {
                            is Edition8Result.Ok -> {
                                setDetail(result.body, 4000)
                                row.setState("Captured — bounded daemon screenshot artifact", SemanticState.POSITIVE)
                                setStatus("Captured — ${responseSummary(result.body)}", SemanticState.POSITIVE)
                            }
                            is Edition8Result.Failed -> row.setState("Failed — ${result.reason}", SemanticState.NEGATIVE)
                            is Edition8Result.Refused -> row.setState("Disabled — ${result.reason}", SemanticState.DIM)
                        }
                    }
                } else {
                    daemon.browserObservations(browserId) { result ->
                        when (result) {
                            is Edition8Result.Ok -> {
                                setDetail(result.body, 4000)
                                row.setState("Read — live console/network observations", SemanticState.POSITIVE)
                                setStatus("Read — ${responseSummary(result.body)}", SemanticState.POSITIVE)
                            }
                            is Edition8Result.Failed -> row.setState("Failed — ${result.reason}", SemanticState.NEGATIVE)
                            is Edition8Result.Refused -> row.setState("Disabled — ${result.reason}", SemanticState.DIM)
                        }
                    }
                }
            }
            return
        }
        if (row.control.id == "X-06") {
            val url = row.value().trim()
            if (url.isEmpty()) {
                row.setState("Disabled — enter the destination URL before connecting", SemanticState.DIM)
                setStatus("Disabled — a destination URL is required", SemanticState.DIM)
                return
            }
            setStatus("Connecting browser…", SemanticState.WARNING)
            row.setState("Connecting — daemon validation and launch", SemanticState.WARNING)
            daemon.connectBrowser(url) { result ->
                when (result) {
                    is Edition8Result.Ok -> {
                        setDetail(result.body, 4000)
                        row.setState("Connected — real browser session", SemanticState.POSITIVE)
                        setStatus("Connected — ${responseSummary(result.body)}", SemanticState.POSITIVE)
                    }
                    is Edition8Result.Failed -> {
                        row.setState("Failed — ${result.reason}", SemanticState.NEGATIVE)
                        setStatus("Unavailable — ${result.reason}", SemanticState.NEGATIVE)
                    }
                    is Edition8Result.Refused -> {
                        row.setState("Disabled — ${result.reason}", SemanticState.DIM)
                        setStatus("Disabled — ${result.reason}", SemanticState.DIM)
                    }
                }
            }
            return
        }
        if (row.control.id == "X-11") {
            val action = host.inlineCodeAction
            if (action == null) {
                row.setState("Disabled — no inline code-action host callback", SemanticState.DIM)
                setStatus("Disabled — inline code actions need a host editor callback", SemanticState.DIM)
                return
            }
            val captured = try {
                action()
            } catch (failure: Throwable) {
                "contract-required: inline code action failed: ${failure.message ?: failure.javaClass.simpleName}"
            }
            setDetail(captured, 4000)
            row.setState("Host — inline code actions", SemanticState.POSITIVE)
            setStatus("Inline code actions — $captured", SemanticState.POSITIVE)
            return
        }
        if (row.field != null || row.toggle != null) {
            stage(row)
            if (row.control.configKeys.isNotEmpty()) {
                setStatus("Staged — use Apply to commit", SemanticState.WARNING)
                return
            }
        }
        setStatus("Loading ${row.control.id}…", SemanticState.WARNING)
        row.setState("Loading… (bounded)", SemanticState.WARNING)
        daemon.readForControl(row.control) { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    val text = responseSummary(result.body)
                    setDetail(result.body, 4000)
                    val source = daemon.endpointFor(row.control) ?: Edition8Stated.sourceOf(row.control)
                    row.setState("Read — $source", SemanticState.POSITIVE)
                    setStatus("Read ${row.control.id} — $text", SemanticState.POSITIVE)
                }
                is Edition8Result.Failed -> {
                    row.setState("Failed — ${result.reason}", SemanticState.NEGATIVE)
                    setStatus("Unavailable — ${result.reason}", SemanticState.NEGATIVE)
                }
                is Edition8Result.Refused -> {
                    row.setState("Disabled — ${result.reason}", SemanticState.DIM)
                    setStatus("Disabled — ${result.reason}", SemanticState.DIM)
                }
            }
        }
    }

    private fun firstMcpServerId(result: Edition8Result): String? {
        if (result !is Edition8Result.Ok) return null
        return try {
            val root = JsonCodec.parse(result.body)
            (root.view("servers").value as? JsonValue.Arr)?.items
                ?.firstOrNull()
                ?.let { (it as? JsonValue.Obj)?.fields?.get("id") as? JsonValue.Str }
                ?.value
        } catch (ignored: Exception) {
            null
        }
    }

    private fun firstBrowserSessionId(result: Edition8Result): String? {
        if (result !is Edition8Result.Ok) return null
        return try {
            val root = JsonCodec.parse(result.body)
            (root.view("sessions").value as? JsonValue.Arr)?.items
                ?.firstOrNull()
                ?.let { (it as? JsonValue.Obj)?.fields?.get("id") as? JsonValue.Str }
                ?.value
        } catch (ignored: Exception) {
            null
        }
    }

    private fun applySettings() {
        if (daemon.draft.changes().isEmpty()) {
            setStatus("Nothing is staged — edit a value before Apply", SemanticState.DIM)
            refreshApply()
            return
        }
        setStatus("Validating one atomic patch…", SemanticState.WARNING)
        daemon.applyConfig { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    val text = responseSummary(result.body)
                    setDetail(result.body, 4000)
                    setStatus("Applied — revision ${daemon.draft.revision}", SemanticState.POSITIVE)
                    for (row in rows) {
                        if (row.field != null || row.toggle != null) {
                            row.setState("Applied · revision ${daemon.draft.revision}", SemanticState.POSITIVE)
                        }
                    }
                    refreshApply()
                }
                is Edition8Result.Failed -> {
                    setStatus("Failed — ${result.reason}; nothing was saved", SemanticState.NEGATIVE)
                    refreshApply()
                }
                is Edition8Result.Refused -> {
                    setStatus("Refused — ${result.reason}; nothing was saved", SemanticState.NEGATIVE)
                    refreshApply()
                }
            }
        }
    }

    private fun runPrimary() {
        if (closed) return
        if (applyIsPrimary) {
            applySettings()
            return
        }
        setStatus("Loading… (${primary.text})", SemanticState.WARNING)
        when (screen.id) {
            "S34" -> hostResult(host.startService, "contract-required: no start-service host action")
            "S35" -> {
                val path = rows.firstOrNull { it.control.id == "O-04" }?.value().orEmpty()
                if (path.isBlank()) setStatus("Disabled — select the executable path before confirming trust", SemanticState.DIM)
                else hostResult({ host.confirmTrust?.invoke(path) }, "contract-required: no trust host action")
            }
            "S36", "S38", "S58" -> {
                val goal = findTextArea(this, "edition8-goal")?.text.orEmpty()
                if (goal.isBlank()) {
                    setStatus("Disabled — a task goal is required; the draft is preserved", SemanticState.DIM)
                } else {
                    val options = rows.filter { it.field != null }
                        .associate { it.control.id to it.value() }
                    hostResult({ host.startTask?.invoke(goal, options) }, "contract-required: no task-start host action")
                }
            }
            "S14" -> {
                setStatus("Reading pending approvals…", SemanticState.WARNING)
                daemon.call("GET", permissionPath(), null) { result ->
                    if (result !is Edition8Result.Ok) {
                        setStatus(statusFor(result), SemanticState.NEGATIVE)
                        return@call
                    }
                    val permission = firstPermission(result.body)
                    if (permission == null) {
                        setStatus("No pending approval — nothing was allowed or faked", SemanticState.DIM)
                    } else {
                        // Review only: the decision stays on the per-request
                        // Allow/Deny card. This action never approves an
                        // unseen request or a different first row.
                        val entry = NativePermissionEntry(
                            permission.id,
                            permission.session,
                            permission.capability,
                            permission.detail
                        )
                        val review = "Review request ${entry.id}: " +
                            "${bound(approvalHeadlineOf(entry), 240)} — no decision was sent; " +
                            "use the approval card's Allow/Deny"
                        val open = host.openApprovals
                        if (open == null) {
                            setStatus("$review (no approval-surface host action)", SemanticState.WARNING)
                        } else {
                            val ack = try {
                                open()
                            } catch (failure: Throwable) {
                                "contract-required: approval surface failed: " +
                                    (failure.message ?: failure.javaClass.simpleName)
                            }
                            setDetail(ack, 4000)
                            if (ack.contains("contract-required", ignoreCase = true) ||
                                ack.contains("failed", ignoreCase = true)
                            ) {
                                setStatus("Disabled — $ack; no decision was sent", SemanticState.DIM)
                            } else {
                                setStatus(review, SemanticState.WARNING)
                            }
                        }
                    }
                }
            }
            "S70" -> {
                if (confirmDestructive(
                        "Stop run?",
                        "Queued work is cancelled. The current response and owned process remain separate. " +
                            "Issued commands and network calls cannot be undone."
                    )
                ) {
                    hostResult({ host.stopScope?.invoke("run") }, "contract-required: no scoped-stop host action")
                } else {
                    setStatus("Cancelled — nothing was stopped", SemanticState.DIM)
                }
            }
            "S24", "S43", "S44" ->
                hostResult({ host.openDiff?.invoke(screen.id) }, "contract-required: no native DiffManager host action")
            "S19" -> hostResult(host.openTerminal, "contract-required: no console host action")
            "S20" -> rows.firstOrNull { it.control.id == "X-08" }?.let { activate(it) }
                ?: setStatus("Disabled — no browser capture control is bound", SemanticState.DIM)
            "S02" -> navigateOr("S05")
            "S06" -> navigateOr("S07")
            "S07" -> navigateOr("S06")
            "S09" -> navigateOr("S08")
            "S22" -> navigateOr("S14")
            "S51" -> navigateOr("S23")
            "S41", "S42", "S32" -> navigateOr("S45")
            "S45" -> navigateOr("S46")
            "S65" -> navigateOr("S24")
            "S27" -> navigateOr("S53")
            "S53" -> navigateOr("S27")
            "S67" -> navigateOr("S18")
            "S31" -> navigateOr("S34")
            "S37" -> navigateOr("S01")
            "S61" -> {
                if (!navigateOr("S01")) {
                    setStatus("Disabled — open Work from the host to start the first task", SemanticState.DIM)
                }
            }
            "S56" -> hostResult(host.restartService, "contract-required: no restart-service host action")
            "S10" -> navigateOr("S08")
            "S11" -> navigateOr("S13")
            "S15" -> navigateOr("S16")
            "S17" -> primaryRead()
            "S23", "S50" -> primaryRead()
            "S25" -> primaryRead()
            "S26", "S71", "S72" -> primaryRead()
            "S33", "S39", "S40", "S59" -> navigateOr("S01")
            "S47", "S48" -> primaryRead()
            "S49" -> setStatus("Disabled — no kept work is bound; nothing was restored", SemanticState.DIM)
            "S52" -> primaryRead()
            "S55", "S68" -> navigateOr("S29")
            "S66" -> {
                if (confirmDestructive(
                        "Delete memory?",
                        "Deletion removes the selected memory from this scope and cannot be undone. " +
                            "No row is selected on this screen."
                    )
                ) {
                    setStatus("Disabled — deletion needs a selected memory row; nothing was deleted", SemanticState.DIM)
                } else {
                    setStatus("Cancelled — nothing was deleted", SemanticState.DIM)
                }
            }
            "S69" -> navigateOr("S11")
            "S54" -> primaryRead()
            else -> primaryRead()
        }
    }

    private fun navigateOr(target: String): Boolean {
        if (host.navigator == null && host.navigate == null) {
            setStatus("Disabled — $target is host-owned; no navigation was made", SemanticState.DIM)
            return false
        }
        host.navigateTo(target)
        return true
    }

    private fun confirmDestructive(title: String, message: String): Boolean {
        if (java.awt.GraphicsEnvironment.isHeadless()) return false
        return javax.swing.JOptionPane.showConfirmDialog(
            this,
            message,
            title,
            javax.swing.JOptionPane.YES_NO_OPTION,
            javax.swing.JOptionPane.WARNING_MESSAGE
        ) == javax.swing.JOptionPane.YES_OPTION
    }

    private fun primaryRead() {
        val path = primaryRoute(screen.id)
        if (path == null) {
            if (rows.any { it.field != null }) {
                applySettings()
            } else {
                setStatus("Disabled — ${screen.id} has no acknowledged native primary route", SemanticState.DIM)
            }
            return
        }
        val resolved = daemon.resolve(path)
        if (resolved == null) {
            setStatus("Disabled — missing prerequisite: session/task scope not bound", SemanticState.DIM)
            return
        }
        daemon.primaryRead(resolved) { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    setDetail(result.body, 4000)
                    setStatus("Read — native response ready", SemanticState.POSITIVE)
                }
                is Edition8Result.Failed -> setStatus("Unavailable — ${result.reason}", SemanticState.NEGATIVE)
                is Edition8Result.Refused -> setStatus("Disabled — ${result.reason}", SemanticState.DIM)
            }
        }
    }

    private fun hostResult(call: (() -> String?)?, missing: String) {
        if (call == null) {
            setStatus("Disabled — $missing", SemanticState.DIM)
            return
        }
        host.work.submit("host action", {
            call() ?: throw IllegalStateException("host action returned no acknowledgement")
        }) { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    val ack = result.body
                    setDetail(ack, 4000)
                    val lower = ack.lowercase()
                    when {
                        lower.contains("contract-required") || lower.contains("nothing") ||
                            lower.contains("no owned") || lower.contains("refused") ||
                            lower.contains("not available") ->
                            setStatus("Disabled — $ack", SemanticState.DIM)
                        lower.contains("awaiting") || lower.contains("requested") ->
                            setStatus("Pending — $ack", SemanticState.WARNING)
                        else -> setStatus("Acknowledged — $ack", SemanticState.POSITIVE)
                    }
                }
                is Edition8Result.Failed -> setStatus("Failed — ${result.reason}", SemanticState.NEGATIVE)
                is Edition8Result.Refused -> setStatus("Refused — ${result.reason}", SemanticState.NEGATIVE)
            }
        }
    }

    private fun permissionPath(): String {
        val sid = host.sessionId()
        return if (sid == null) "/native/permissions" else "/native/permissions?session=$sid"
    }

    private fun firstPermission(body: String): PermissionDisplayRow? {
        val permissions = runCatching {
            JsonCodec.parse(body).view("GET /native/permissions").field("permissions").array()
        }.getOrNull().orEmpty()
        val first = permissions.firstOrNull() ?: return null
        val id = first.optionalField("id")?.string() ?: return null
        val session = first.optionalField("session_id")?.string() ?: host.sessionId() ?: return null
        val capability = first.optionalField("capability")?.string() ?: "unknown"
        val detail = first.optionalField("detail")?.let { d ->
            (d.value as? JsonValue.Str)?.value ?: d.rawJson()
        } ?: ""
        return PermissionDisplayRow(id, session, capability, detail)
    }

    fun setStatus(text: String, state: SemanticState) {
        val full = bound(text, 240)
        fullStatus = full
        val short = shortStatus(full)
        status.text = short
        status.foreground = semanticForeground(state)
        status.toolTipText = bound(full, 800)
        status.accessibleContext?.accessibleName = "Status: $short"
        val detail = when {
            full.startsWith("Disabled — ") -> full.removePrefix("Disabled — ")
            full.startsWith("Unavailable — ") -> full.removePrefix("Unavailable — ")
            full.startsWith("Failed — ") -> full.removePrefix("Failed — ")
            full.startsWith("Refused — ") -> full.removePrefix("Refused — ")
            full.startsWith("Pending — ") -> full.removePrefix("Pending — ")
            full.startsWith("Acknowledged — ") -> full.removePrefix("Acknowledged — ")
            full.startsWith("Staged") -> full.substringAfter("— ", full)
            full.startsWith("Loading settings") -> "Reading the configuration snapshot; no value is invented."
            else -> full
        }
        statusDetail.text = if (detail == short || detail == full && short == full) "" else detail
        statusDetail.isVisible = statusDetail.text.isNotEmpty()
        revalidate()
    }

    private fun shortStatus(text: String): String = when {
        text.startsWith("Loading settings") -> "Loading settings"
        text.startsWith("Disabled") -> "Disabled"
        text.startsWith("Unavailable") -> "Unavailable"
        text.startsWith("Settings read") -> "Settings read"
        text.startsWith("No unsaved") -> "No unsaved changes"
        text.startsWith("Staged") -> {
            val count = Regex("Staged (\\d+) change").find(text)?.groupValues?.getOrNull(1)
            if (count == null) "Staged changes" else "Staged $count"
        }
        text.startsWith("Applied") -> "Applied"
        text.startsWith("Acknowledged") -> "Acknowledged"
        text.startsWith("Pending") -> "Pending"
        text.startsWith("Failed") -> "Failed"
        text.startsWith("Refused") -> "Refused"
        text.startsWith("Host action") -> "Host action"
        text.startsWith("Read") -> "Read"
        text.startsWith("Captured") -> "Captured"
        text.startsWith("Connected") -> "Connected"
        text.startsWith("No pending") -> "No pending approval"
        text.startsWith("Review request") -> "Review only"
        text.startsWith("Validating") -> "Validating"
        else -> bound(text, 28)
    }

    fun shutdown() {
        closed = true
        daemon.work.close()
    }

    internal fun rowsForTest(): List<Edition8ControlRow> = rows

    internal fun statusForTest(): String = fullStatus

    internal fun applyVisibleForTest(): Boolean = apply.isVisible || (applyIsPrimary && apply.isEnabled)

    private fun statusFor(result: Edition8Result): String = when (result) {
        is Edition8Result.Ok -> "Read"
        is Edition8Result.Failed -> "Unavailable — ${result.reason}"
        is Edition8Result.Refused -> "Disabled — ${result.reason}"
    }

    private fun responseSummary(body: String): String {
        val flat = body.replace(Regex("\\s+"), " ").trim()
        return bound(flat, 160)
    }

    private fun findTextArea(root: Component, name: String): JTextArea? {
        if (root is JTextArea && root.name == name) return root
        if (root is Container) {
            for (child in root.components) findTextArea(child, name)?.let { return it }
        }
        return null
    }

    private fun bindCmdEnter(run: () -> Unit) {
        val key = KeyStroke.getKeyStroke(
            KeyEvent.VK_ENTER,
            java.awt.Toolkit.getDefaultToolkit().menuShortcutKeyMaskEx
        )
        inputMap.put(key, "edition8.run")
        actionMap.put("edition8.run", object : javax.swing.AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) = run()
        })
    }

    companion object {
        fun primaryRoute(screenId: String): String? = when (screenId) {
            "S01" -> "/native/session/{id}/task-runs"
            "S03", "S04" -> "/native/providers/connections"
            "S05" -> "/native/model-parameters"
            "S08", "S62", "S09" -> "/native/session/{id}/context/preview"
            "S10" -> "/native/session/{id}/context/preview"
            "S11", "S69" -> "/native/config/snapshot"
            "S12", "S66" -> "/native/memory"
            "S13" -> "/native/permissions/policy"
            "S14" -> "/native/permissions"
            "S15", "S16", "S68" -> "/native/mcp/servers"
            "S17" -> "/native/skills"
            "S18", "S67" -> "/native/hooks"
            "S19" -> "/native/terminals?session={id}"
            "S20" -> "/native/browser/status"
            "S21", "S28", "S29", "S60", "S64" -> "/native/config/snapshot"
            "S22" -> "/native/permissions"
            "S23", "S50" -> "/native/agents?session={id}"
            "S24", "S43" -> "/native/tasks/{id}/completion-steps"
            "S25" -> "/native/session/{id}/checkpoints"
            "S26", "S61", "S71", "S72" -> "/native/sessions"
            "S27" -> "/native/session/{id}/usage"
            "S30", "S56" -> "/native/diagnostics/preview"
            "S31" -> "/native/session/{id}/verification"
            "S32", "S65", "S41", "S42", "S45", "S46", "S47", "S48" -> "/native/tasks/{id}/proof"
            "S33", "S36", "S37", "S38", "S39", "S34", "S35", "S57", "S58", "S59", "S70" -> null
            "S51" -> "/native/session/{id}/tournaments"
            "S52" -> "/native/board?session={id}"
            "S53" -> "/native/outcomes?limit=50"
            "S54" -> "/native/session/{id}/usage"
            "S55" -> "/native/identity"
            "S63" -> "/native/model-parameters"
            else -> null
        }
    }
}

/** Work / Inspect / History: one tool window, exactly three clusters. */
internal class Edition8ShellPanel(private val host: Edition8Host) : JPanel(BorderLayout()) {

    private val cards = JPanel(CardLayout())
    private val tabs = JTabbedPane()
    private val chooser = JComboBox<String>()
    private val built = LinkedHashMap<String, JComponent>()
    private var opening = false
    private val clusterHosts =
        LinkedHashMap<String, Pair<JComboBox<String>, JPanel>>()

    init {
        name = "edition8-shell"
        a11y("Faktor Work Inspect History", "Revision 8 shell with three destinations.")
        val groups = linkedMapOf(
            "Work" to listOf("Setup and start", "Work"),
            "Inspect" to listOf(
                "Inspect and proof", "Agents", "Context", "Instructions and memory",
                "Models and providers", "Permissions and tools"
            ),
            "History" to listOf("History", "Usage", "Settings and diagnostics")
        )
        for ((cluster, screenGroups) in groups) {
            val content = JPanel(BorderLayout())
            content.isOpaque = false
            val localChooser = JComboBox(
                Edition8Registry.screens.filter { it.group in screenGroups }
                    .map { "${it.id} · ${it.title}" }.toTypedArray()
            )
            localChooser.a11y("$cluster screens", "Chooses one Revision 8 screen in the $cluster destination.")
            localChooser.addActionListener { event ->
                if (opening) return@addActionListener
                val selected = localChooser.selectedItem?.toString() ?: return@addActionListener
                open(selected.substringBefore(" ·"))
            }
            val hostPanel = JPanel(BorderLayout())
            hostPanel.isOpaque = false
            hostPanel.name = "${cluster.lowercase()}-host"
            hostPanel.add(localChooser, BorderLayout.NORTH)
            hostPanel.add(JPanel(BorderLayout()), BorderLayout.CENTER)
            val chooserHosts = localChooser to hostPanel
            content.add(localChooser, BorderLayout.NORTH)
            content.add(hostPanel, BorderLayout.CENTER)
            clusterHosts[cluster] = chooserHosts
            tabs.addTab(cluster, content)
        }
        tabs.a11y("Work Inspect History destinations", "Three content clusters; Settings opens from the toolbar.")
        tabs.addChangeListener {
            if (opening) return@addChangeListener
            val title = tabs.getTitleAt(tabs.selectedIndex)
            val pair = clusterHosts[title] ?: return@addChangeListener
            val selected = pair.first.selectedItem?.toString()?.substringBefore(" ·") ?: return@addChangeListener
            open(selected)
        }
        add(tabs, BorderLayout.CENTER)
        open(defaultScreen("Work"))
    }

    private fun defaultScreen(cluster: String): String = when (cluster) {
        "Inspect" -> "S41"
        "History" -> "S26"
        else -> "S34"
    }

    fun open(screenId: String) {
        val id = if (screenId in Edition8Registry.screens.map { it.id }) screenId else "S34"
        val panel = built.getOrPut(id) { Edition8ScreenRegistry.build(id, host) }
        val screen = Edition8Registry.screen(id)
        val cluster = when (screen.group) {
            "Setup and start", "Work" -> "Work"
            "History", "Usage", "Settings and diagnostics" -> "History"
            else -> "Inspect"
        }
        val pair = clusterHosts[cluster] ?: return
        val index = tabs.indexOfTab(cluster)
        opening = true
        try {
            if (tabs.selectedIndex != index) tabs.selectedIndex = index
            val wanted = "${screen.id} · ${screen.title}"
            for (i in 0 until pair.first.itemCount) {
                if (pair.first.getItemAt(i) == wanted) pair.first.selectedIndex = i
            }
        } finally {
            opening = false
        }
        val holder = pair.second
        holder.removeAll()
        holder.add(panel, BorderLayout.CENTER)
        holder.revalidate()
        holder.repaint()
    }

    fun shutdown() {
        for (panel in built.values) {
            if (panel is Edition8ScreenPanel) panel.shutdown()
        }
        host.shutdown()
    }
}

/** A registry-driven chooser for the real tool window's toolbar. */
internal class Edition8ScreenBrowser(private val host: Edition8Host) {

    fun show(parent: Component?, initialScreen: String? = null) {
        val dialog = JDialog(
            SwingUtilities.getWindowAncestor(parent),
            "Faktor",
            java.awt.Dialog.ModalityType.APPLICATION_MODAL
        )
        dialog.defaultCloseOperation = WindowConstants.DISPOSE_ON_CLOSE
        val shell = Edition8ShellPanel(host)
        // The shell owns in-dialog navigation; Back closes this browser and
        // returns to the workbench instead of opening a nested modal copy.
        host.navigator = { id -> shell.open(id) }
        host.backHandler = { dialog.dispose() }
        val settings = secondaryButton("Settings")
        settings.name = "edition8-browser-settings"
        settings.a11y("Open Settings", "Opens General settings inside this browser.")
        settings.addActionListener { shell.open("S29") }
        val close = secondaryButton("Close")
        close.a11y("Close Faktor screens", "Returns to the workbench; the draft is untouched.")
        close.addActionListener { dialog.dispose() }
        val toolbar = wrapRow(settings, close)
        toolbar.border = BorderFactory.createEmptyBorder(Spacing.S, Spacing.S, Spacing.S, Spacing.S)
        val content = JPanel(BorderLayout())
        content.add(toolbar, BorderLayout.NORTH)
        content.add(shell, BorderLayout.CENTER)
        dialog.contentPane = content
        if (initialScreen != null) shell.open(initialScreen)
        val escape = KeyStroke.getKeyStroke(KeyEvent.VK_ESCAPE, 0)
        dialog.rootPane.getInputMap(JComponent.WHEN_IN_FOCUSED_WINDOW).put(escape, "edition8.close")
        dialog.rootPane.actionMap.put("edition8.close", object : javax.swing.AbstractAction() {
            override fun actionPerformed(e: java.awt.event.ActionEvent?) = dialog.dispose()
        })
        dialog.setSize(960, 760)
        dialog.setLocationRelativeTo(parent)
        dialog.addWindowListener(object : java.awt.event.WindowAdapter() {
            override fun windowClosed(e: java.awt.event.WindowEvent?) {
                shell.shutdown()
            }
        })
        dialog.isVisible = true
    }
}
