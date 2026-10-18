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
    val openDiff: ((String) -> String)? = null,
    val openTerminal: (() -> String)? = null,
    val navigate: ((String) -> Unit)? = null,
    val notice: (String) -> Unit = {},
    val credentials: ControlPlaneCredentialStore? = null
) {
    val work = Edition8Work()

    fun daemon(): Edition8Daemon = Edition8Daemon(client, sessionId, taskId, work)

    fun shutdown() = work.close()
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
) : JPanel(GridBagLayout()) {

    val field: JTextField? = if (control.configKeys.isNotEmpty()) JTextField(24) else null
    val toggle: JCheckBox? = if (isToggle(control) && control.configKeys.isNotEmpty()) JCheckBox() else null
    val probe: JButton = if (control.endpoints.any { it.state == "exists" } ||
        control.configKeys.isEmpty()
    ) {
        secondaryButton("Read")
    } else {
        secondaryButton("Read")
    }
    private val stateLabel = JLabel(Edition8Stated.initial(control))
    private val configKey: String? = control.configKeys.firstOrNull()

    init {
        isOpaque = false
        border = BorderFactory.createEmptyBorder(0, 0, Spacing.S, 0)
        val label = FixedWrapLabel("${control.id}  ${control.name}")
        label.font = label.font.deriveFont(label.font.style or java.awt.Font.BOLD)
        val detail = wrappedMutedLabel("${control.control} · ${control.applyTiming}")
        val left = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            add(label)
            add(detail)
        }
        val center = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            when {
                toggle != null -> add(wrapRow(toggle))
                field != null -> {
                    field.name = "edition8-field-${control.id}"
                    field.a11y(
                        "${control.id} ${control.name}",
                        "Value for ${control.control}. ${control.applyTiming}."
                    )
                    field.text = control.defaultValue.takeUnless { it.equals("none", true) } ?: ""
                    if (configKey != null && panel.daemon.draft.isLocked(configKey)) {
                        field.isEnabled = false
                        field.toolTipText = "Set by managed policy; read-only"
                    }
                    add(field)
                }
                else -> add(mutedLabel("Read-only contract"))
            }
            add(stateLabel)
        }
        probe.name = "edition8-activate-${control.id}"
        probe.a11y(
            "${control.id} ${control.name}: ${control.control}",
            "Reads or applies ${control.name}. ${control.behavior}"
        )
        probe.addActionListener { panel.activate(this) }
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

        val constraints = GridBagConstraints().apply {
            gridx = 0
            gridy = 0
            weightx = 0.0
            fill = GridBagConstraints.HORIZONTAL
            anchor = GridBagConstraints.NORTHWEST
            insets = Insets(0, 0, 0, Spacing.M)
        }
        add(left, constraints)
        add(
            center,
            GridBagConstraints().apply {
                gridx = 1
                gridy = 0
                weightx = 1.0
                fill = GridBagConstraints.HORIZONTAL
                anchor = GridBagConstraints.NORTHWEST
            }
        )
        add(
            probe,
            GridBagConstraints().apply {
                gridx = 2
                gridy = 0
                anchor = GridBagConstraints.NORTHEAST
            }
        )
        a11y("${control.id} ${control.name}", control.stateContract)
    }

    fun value(): String = toggle?.let { if (it.isSelected) "true" else "false" }
        ?: field?.text ?: ""

    fun setState(text: String, state: SemanticState) {
        stateLabel.text = text
        stateLabel.foreground = semanticForeground(state)
        stateLabel.toolTipText = text
        stateLabel.accessibleContext?.accessibleDescription = text
    }

    fun stateText(): String = stateLabel.text

    private fun isToggle(control: Edition8Control): Boolean {
        val text = (control.control + " " + control.name + " " + control.behavior).lowercase()
        return text.contains("toggle") || text.contains("checkbox") || text.contains("switch") ||
            control.defaultValue.equals("true", true) || control.defaultValue.equals("false", true)
    }
}

internal object Edition8Stated {
    fun initial(control: Edition8Control): String =
        if (control.configKeys.isEmpty()) "Not read" else "Staged value: not saved"

    fun sourceOf(control: Edition8Control): String =
        control.endpoints.firstOrNull { it.state == "exists" }?.actual
            ?: control.endpoints.firstOrNull()?.declared
            ?: "no native route named"
}

/** One real screen. All I/O is delegated to [Edition8Daemon] off the EDT. */
internal class Edition8ScreenPanel(
    val screen: Edition8Screen,
    val host: Edition8Host
) : JPanel(BorderLayout()) {

    val daemon: Edition8Daemon = host.daemon()
    val rows: List<Edition8ControlRow>
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
    private val primary: JButton = primaryButton(screen.primaryAction.removeSuffix(" · JetBrains: OK / Apply"))
    private val apply: JButton = secondaryButton("Apply settings")
    private val back: JButton = secondaryButton("Back")
    private val progress = JLabel("")
    private var closed = false
    private var loadingValues = false

    init {
        rows = Edition8Registry.controlsForScreen(screen.id).map { Edition8ControlRow(it, this) }
        val header = buildHeader()
        val body = JPanel(BorderLayout()).apply {
            isOpaque = false
            add(buildHero(), BorderLayout.NORTH)
            add(buildForm(), BorderLayout.CENTER)
        }
        val scroll = pageScroll(body)
        add(header, BorderLayout.NORTH)
        add(scroll, BorderLayout.CENTER)
        add(buildFooter(), BorderLayout.SOUTH)
        primary.name = "edition8-primary-${screen.id}"
        primary.a11y(
            "${screen.id} ${screen.primaryAction}",
            "Primary action for ${screen.title}. ${screen.states}"
        )
        primary.addActionListener { runPrimary() }
        apply.name = "edition8-apply-${screen.id}"
        apply.a11y(
            "Apply ${screen.title} settings",
            "Commits one atomic configuration patch and waits for the daemon acknowledgement."
        )
        apply.isVisible = rows.any { it.field != null || it.toggle != null }
        apply.addActionListener { applySettings() }
        back.a11y("Back from ${screen.title}", "Returns to the invoking screen with the draft intact.")
        back.addActionListener { host.navigate?.invoke("S01") ?: setStatus("Back is a host navigation action", SemanticState.DIM) }
        installEscape()
        loadConfigIfNeeded()
    }

    private fun buildHeader(): JComponent {
        val title = JLabel("${screen.id} · ${screen.title}").apply {
            font = sectionTitleFont()
        }
        status.name = "edition8-status-${screen.id}"
        status.a11y("Status: ${status.text}", "One status line for ${screen.title}.")
        val meta = mutedLabel(
            "${screen.group} · design p.${screen.designPage} · contract p.${screen.contractPage} · " +
                screen.surface.name.lowercase().replace('_', ' ')
        )
        val column = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            add(title)
            add(meta)
            add(wrapRow(status, progress))
        }
        return JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(Spacing.L, Spacing.L, Spacing.S, Spacing.L)
            add(column, BorderLayout.WEST)
            add(primary, BorderLayout.EAST)
        }
    }

    private fun buildHero(): JComponent {
        val hero = JPanel().apply {
            isOpaque = false
            layout = BoxLayout(this, BoxLayout.Y_AXIS)
            border = BorderFactory.createEmptyBorder(0, Spacing.L, Spacing.S, Spacing.L)
        }
        when (screen.id) {
            "S34", "S35", "S57" -> {
                hero.add(wrappedMutedLabel("Set up Faktor — the service, executable trust and shell isolation are separate facts."))
                hero.add(wrapRow(
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
                    a11y("Task goal", "One task. Cmd+Enter runs; the draft survives admission.")
                    bindCmdEnter { runPrimary() }
                }
                hero.add(goal)
                hero.add(wrappedMutedLabel(
                    "Captured at admission: intent, model, effort, authority, context, criteria, " +
                        "finish level and run caps. Starting duplicate suppression stays on until the receipt."
                ))
            }
            "S41", "S42", "S45", "S46", "S47", "S48", "S32", "S65", "S24", "S43", "S44" -> {
                hero.add(wrappedMutedLabel(
                    "Four completion boundaries stay separate: required acceptance, claim evidence, " +
                        "matching integration and requested finish. A chat claim never fills a boundary."
                ))
            }
            "S54", "S53", "S27" -> {
                hero.add(wrappedMutedLabel(
                    "Provider API spend, subscription quota, reservations and local operating cost are " +
                        "separate categories. Unavailable is not zero and no verified change is not $0."
                ))
            }
            "S10", "S08", "S62", "S09" -> {
                hero.add(wrappedMutedLabel(
                    "Capacity and input usage come from the durable context manifest. A draft estimate " +
                        "is labelled with its own basis; compaction is a durable operation with a minimum reduction."
                ))
            }
            "S03", "S04", "S05", "S02", "S63" -> {
                hero.add(wrappedMutedLabel(
                    "A provider/model pair is captured only after the daemon acknowledges it. Secrets " +
                        "are PasswordSafe references; no secret value is written to shared config."
                ))
            }
            "S19", "S20" -> {
                hero.add(wrappedMutedLabel(
                    "Owned process and browser observations are bounded artifacts. Run cancellation, " +
                        "terminal kill and session abort are separate operations."
                ))
            }
        }
        hero.add(detailScroll.apply { preferredSize = Dimension(10, 96) })
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
        val column = pageColumn(gap = Spacing.XS, padding = Spacing.L)
        column.add(sectionHeader("Controls (${rows.size})"))
        for (row in rows) column.add(row)
        column.add(wrappedMutedLabel(
            "Every row names its apply timing and source. Read calls the exact served contract; " +
                "a route absent from the daemon stays a typed contract gap, never a green state."
        ))
        return column
    }

    private fun buildFooter(): JComponent =
        JPanel(BorderLayout()).apply {
            isOpaque = false
            border = BorderFactory.createEmptyBorder(Spacing.S, Spacing.L, Spacing.L, Spacing.L)
            add(wrapRow(apply, back), BorderLayout.WEST)
            add(mutedLabel("OK / Cancel / Apply: staging is local; Apply is atomic."), BorderLayout.EAST)
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
                            val field = row.field ?: continue
                            val value = daemon.draft.value(key)
                            if (value.isNotEmpty()) field.text = value
                            row.setState(
                                if (daemon.draft.isLocked(key)) "Locked by organization — read-only"
                                else "Effective revision ${daemon.draft.revision} — staged edits not saved",
                                if (daemon.draft.isLocked(key)) SemanticState.DIM else SemanticState.POSITIVE
                            )
                        }
                        setStatus("Settings read — revision ${daemon.draft.revision}", SemanticState.POSITIVE)
                    } catch (t: Throwable) {
                        setStatus("Unavailable — ${Edition8Work.describe(t)}", SemanticState.NEGATIVE)
                    } finally {
                        loadingValues = false
                    }
                }
                is Edition8Result.Failed -> setStatus("Unavailable — ${result.reason}", SemanticState.NEGATIVE)
                is Edition8Result.Refused -> setStatus("Disabled — ${result.reason}", SemanticState.DIM)
            }
        }
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
        row.setState("Staged — apply commits one atomic patch", SemanticState.WARNING)
        setStatus(
            if (daemon.draft.changes().isEmpty()) "No unsaved changes (revision ${daemon.draft.revision})"
            else "Staged ${daemon.draft.changes().size} change(s) — not saved",
            SemanticState.WARNING
        )
    }

    fun activate(row: Edition8ControlRow) {
        if (closed) return
        if (row.field != null || row.toggle != null) {
            stage(row)
            val key = row.control.configKeys.firstOrNull()
            if (key != null && row.field != null) {
                daemon.draft.stage(key, row.field.text)
            }
        }
        setStatus("Loading ${row.control.id}…", SemanticState.WARNING)
        row.setState("Loading… (bounded)", SemanticState.WARNING)
        daemon.readForControl(row.control) { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    val text = responseSummary(result.body)
                    detail.text = bound(result.body, 4000)
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

    private fun applySettings() {
        setStatus("Validating one atomic patch…", SemanticState.WARNING)
        daemon.applyConfig { result ->
            when (result) {
                is Edition8Result.Ok -> {
                    val text = responseSummary(result.body)
                    detail.text = bound(result.body, 4000)
                    setStatus("Applied — $text", SemanticState.POSITIVE)
                    for (row in rows) {
                        if (row.field != null || row.toggle != null) {
                            row.setState("Applied at revision ${daemon.draft.revision}", SemanticState.POSITIVE)
                        }
                    }
                }
                is Edition8Result.Failed -> setStatus("Failed — ${result.reason}", SemanticState.NEGATIVE)
                is Edition8Result.Refused -> setStatus("Refused — ${result.reason}", SemanticState.NEGATIVE)
            }
        }
    }

    private fun runPrimary() {
        if (closed) return
        setStatus("Loading… (${screen.primaryAction})", SemanticState.WARNING)
        when (screen.id) {
            "S34" -> hostResult(host.startService, "contract-required: no start-service host action")
            "S35" -> {
                val path = rows.firstOrNull { it.control.id == "O-04" }?.value().orEmpty()
                if (path.isBlank()) setStatus("Disabled — select the executable path before confirming trust", SemanticState.DIM)
                else hostResult({ host.confirmTrust?.invoke(path) }, "contract-required: no trust host action")
            }
            "S57" -> setStatus("Isolation remains fail-closed until the platform setup acknowledges it", SemanticState.DIM)
            "S36", "S38", "S39", "S33", "S58" -> {
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
                setStatus("Loading pending approvals…", SemanticState.WARNING)
                daemon.call("GET", permissionPath(), null) { result ->
                    if (result !is Edition8Result.Ok) {
                        setStatus(statusFor(result), SemanticState.NEGATIVE)
                        return@call
                    }
                    val permission = firstPermission(result.body)
                    if (permission == null) {
                        setStatus("No pending approval — nothing was allowed or faked", SemanticState.DIM)
                    } else {
                        hostResult(
                            { host.replyApproval?.invoke(permission.first, permission.second, "allow") },
                            "contract-required: no approval host action"
                        )
                    }
                }
            }
            "S70" -> hostResult({ host.stopScope?.invoke("run") }, "contract-required: no scoped-stop host action")
            "S24", "S43", "S44" -> hostResult({ host.openDiff?.invoke(screen.id) }, "contract-required: no native DiffManager host action")
            "S19" -> hostResult(host.openTerminal, "contract-required: no console host action")
            "S02", "S06", "S09", "S22", "S23", "S50", "S51", "S52", "S59" -> {
                val target = when (screen.id) {
                    "S02" -> "S05"
                    "S06" -> "S07"
                    "S09" -> "S08"
                    "S22", "S50" -> "S14"
                    "S51", "S52" -> "S23"
                    else -> "S01"
                }
                host.navigate?.invoke(target) ?: setStatus("Navigation target $target is host-owned; draft preserved", SemanticState.DIM)
            }
            "S56" -> hostResult(host.restartService, "contract-required: no restart-service host action")
            else -> primaryRead()
        }
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
                    detail.text = bound(result.body, 4000)
                    setStatus("Read — $resolved · ${responseSummary(result.body)}", SemanticState.POSITIVE)
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
                    detail.text = bound(result.body, 4000)
                    setStatus("Acknowledged — ${responseSummary(result.body)}", SemanticState.POSITIVE)
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

    private fun firstPermission(body: String): Pair<String, String>? {
        val permissions = runCatching {
            JsonCodec.parse(body).view("permissions").optionalField("permissions")?.array().orEmpty()
        }.getOrNull().orEmpty()
        val first = permissions.firstOrNull() ?: return null
        val id = first.optionalField("id")?.string() ?: return null
        val session = first.optionalField("session_id")?.string() ?: host.sessionId() ?: return null
        return id to session
    }

    fun setStatus(text: String, state: SemanticState) {
        status.text = bound(text, 240)
        status.foreground = semanticForeground(state)
        status.toolTipText = bound(text, 800)
        status.accessibleContext?.accessibleName = "Status: ${bound(text, 240)}"
        progress.text = ""
    }

    fun shutdown() {
        closed = true
        daemon.work.close()
    }

    internal fun rowsForTest(): List<Edition8ControlRow> = rows

    internal fun statusForTest(): String = status.text

    internal fun applyVisibleForTest(): Boolean = apply.isVisible

    private fun statusFor(result: Edition8Result): String = when (result) {
        is Edition8Result.Ok -> "Read"
        is Edition8Result.Failed -> result.reason
        is Edition8Result.Refused -> result.reason
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
        tabs.a11y("Work Inspect History destinations", "Three content clusters; no settings center.")
        tabs.addChangeListener {
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
        if (tabs.selectedIndex != index) tabs.selectedIndex = index
        opening = true
        try {
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
            "Faktor screens — Revision 8",
            java.awt.Dialog.ModalityType.APPLICATION_MODAL
        )
        dialog.defaultCloseOperation = WindowConstants.DISPOSE_ON_CLOSE
        val shell = Edition8ShellPanel(host)
        val list = JComboBox(
            Edition8Registry.screens.map { "${it.group} · ${it.id} · ${it.title}" }.toTypedArray()
        )
        list.a11y("All Revision 8 screens", "72 screens; choosing one opens the native surface.")
        list.addActionListener {
            val id = list.selectedItem?.toString()?.split(" · ")?.getOrNull(1) ?: return@addActionListener
            shell.open(id)
        }
        val content = JPanel(BorderLayout())
        content.add(list, BorderLayout.NORTH)
        content.add(shell, BorderLayout.CENTER)
        dialog.contentPane = content
        if (initialScreen != null) shell.open(initialScreen)
        dialog.setSize(920, 720)
        dialog.setLocationRelativeTo(parent)
        dialog.addWindowListener(object : java.awt.event.WindowAdapter() {
            override fun windowClosed(e: java.awt.event.WindowEvent?) {
                shell.shutdown()
            }
        })
        dialog.isVisible = true
    }
}
