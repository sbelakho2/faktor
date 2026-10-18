// Edition 5 configuration center: one shared, application-scoped controller
// over typed descriptors + an atomic validated patch, rendered with a host
// settings tree (S29) and every config-bearing control family (S03-S07,
// S11-S18, S21, S28-S31).
//
// Rules enforced here (pp.013, 026-027, 038-039, 165-166):
//   * a value is EFFECTIVE only after the daemon acknowledges an apply;
//     staged/local edits never masquerade as saved or applied;
//   * validation is complete before the single patch call;
//   * unknown/unavailable contracts render the exact missing route/DTO and
//     preserve the draft (never an inert toggle, never a green toast);
//   * all I/O happens off the EDT; the panel only renders worker results.
package dev.faktor.frontend

import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.Component
import java.awt.Dimension
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Insets
import java.awt.event.ComponentAdapter
import java.awt.event.ComponentEvent
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import javax.swing.BorderFactory
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JPasswordField
import javax.swing.JScrollPane
import javax.swing.JSplitPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.ListSelectionModel
import javax.swing.SwingUtilities
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener

/** One served configuration snapshot (revision + per-field effective values). */
internal data class Edition5ConfigSnapshot(
    val revision: Long,
    val savedRevision: Long?,
    val runtimeRevision: Long?,
    val values: Map<String, String>,
    val sources: Map<String, String>,
    val locked: Set<String>,
    val applyTiming: Map<String, String>
)

internal data class Edition5ConfigError(val key: String?, val code: String, val message: String)

internal data class Edition5ConfigDecision(
    val ok: Boolean,
    val revision: Long?,
    val errors: List<Edition5ConfigError>,
    val applyTiming: Map<String, String>,
    /** Exact contract that is missing when the daemon has no route/DTO. */
    val contractGap: String? = null
)

/**
 * The daemon seam. Implementations must perform real HTTP calls; an absent
 * route is reported as `contractGap` with the exact route/DTO, never as a
 * successful save.
 */
internal interface Edition5ConfigSource {
    fun read(): Edition5ConfigSnapshot

    fun validate(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision

    fun apply(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision

    fun close() {}
}

internal class Edition5ConfigUnavailable(private val reason: String) : Edition5ConfigSource {
    override fun read(): Edition5ConfigSnapshot = throw Edition5ContractRequired(reason)

    override fun validate(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision =
        Edition5ConfigDecision(false, null, emptyList(), emptyMap(), reason)

    override fun apply(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision =
        Edition5ConfigDecision(false, null, emptyList(), emptyMap(), reason)
}

internal class Edition5ContractRequired(override val message: String) : Exception(message)

/** The exact typed apply state a control row renders. */
internal enum class Edition5FieldState(val label: String) {
    EFFECTIVE("Effective"),
    STAGED("Staged"),
    SAVED_NOT_APPLIED("Saved, not applied"),
    NEXT_CALL("Applies next call"),
    NEXT_TASK("Applies next task"),
    NEW_SESSION("Applies next conversation"),
    RESTART_REQUIRED("Restart required"),
    FAILED("Failed"),
    LOCKED("Managed"),
    CONTRACT_REQUIRED("Contract required")
}

internal interface Edition5SettingsStore {
    fun read(key: String): String?

    fun write(values: Map<String, String?>)
}

internal class InMemoryEdition5SettingsStore : Edition5SettingsStore {
    private val values = LinkedHashMap<String, String>()

    override fun read(key: String): String? = values[key]

    override fun write(values: Map<String, String?>) {
        for ((key, value) in values) {
            if (value == null) this.values.remove(key) else this.values[key] = value
        }
    }
}

/**
 * The shared staging/validation controller. It never performs I/O on the
 * caller thread; [readOffEdt] and [applyOffEdt] are worker methods and every
 * result is applied back on the EDT by the panel.
 */
internal class Edition5SettingsController(
    private val source: Edition5ConfigSource,
    private val store: Edition5SettingsStore = InMemoryEdition5SettingsStore(),
    private val endpointLabel: String = "daemon"
) {
    private val lock = Any()

    @Volatile
    var snapshot: Edition5ConfigSnapshot? = null
        private set

    @Volatile
    var loadError: String? = null
        private set

    private val staged = LinkedHashMap<String, String>()
    private val stagedOwner = LinkedHashMap<String, String>()
    private val failed = LinkedHashMap<String, String>()
    private val applied = LinkedHashMap<String, String>()

    fun readOffEdt() {
        try {
            val next = source.read()
            synchronized(lock) {
                snapshot = next
                applied.clear()
                applied.putAll(next.values)
                loadError = null
            }
        } catch (e: Exception) {
            synchronized(lock) {
                loadError = typedReason(e)
            }
        }
    }

    fun value(control: Edition5Control): String {
        val key = keyOf(control)
        synchronized(lock) {
            return staged[key] ?: applied[key] ?: store.read(key) ?: ""
        }
    }

    fun state(control: Edition5Control): Edition5FieldState {
        val key = keyOf(control)
        val snap = snapshot
        synchronized(lock) {
            if (failed.containsKey(key)) return Edition5FieldState.FAILED
            if (staged.containsKey(key)) return Edition5FieldState.STAGED
            if (snap != null && key in snap.locked) return Edition5FieldState.LOCKED
            if (snap != null && key in snap.values) {
                return when (snap.applyTiming[key]?.lowercase()) {
                    "next call" -> Edition5FieldState.NEXT_CALL
                    "next task" -> Edition5FieldState.NEXT_TASK
                    "new conversation", "new session" -> Edition5FieldState.NEW_SESSION
                    "restart required" -> Edition5FieldState.RESTART_REQUIRED
                    else -> Edition5FieldState.EFFECTIVE
                }
            }
        }
        if (snap == null) return Edition5FieldState.CONTRACT_REQUIRED
        return Edition5FieldState.SAVED_NOT_APPLIED
    }

    fun setValue(control: Edition5Control, value: String) {
        val key = keyOf(control)
        synchronized(lock) {
            failed.remove(key)
            val effective = snapshot?.values?.get(key)
            if (effective == value) {
                staged.remove(key)
                stagedOwner.remove(key)
            } else {
                staged[key] = value
                stagedOwner[key] = control.id
            }
        }
    }

    fun changes(): Map<String, String?> = synchronized(lock) { LinkedHashMap(staged) }

    fun changedCount(): Int = synchronized(lock) { staged.size }

    fun revisedCount(): Int = synchronized(lock) { staged.size + failed.size }

    fun errorFor(control: Edition5Control): String? =
        synchronized(lock) { failed[keyOf(control)] }

    /** Complete local validation, including value/syntax checks. */
    fun validateLocal(): List<Edition5ConfigError> {
        val errors = ArrayList<Edition5ConfigError>()
        synchronized(lock) {
            for ((key, value) in staged) {
                val ownerId = stagedOwner[key]
                val control = if (ownerId != null) {
                    Edition5Catalog.byId[ownerId]
                } else {
                    controlFor(key)
                } ?: continue
                val kind = Edition5ControlKinds.of(control)
                if (kind == Edition5ControlKind.NUMBER && value.isNotBlank()) {
                    if (value.toBigIntegerOrNull() == null || value.startsWith("-")) {
                        errors.add(
                            Edition5ConfigError(
                                key, "invalid_integer",
                                "${control.name} must be a non-negative whole number"
                            )
                        )
                    }
                }
                if (kind == Edition5ControlKind.URL && value.isNotBlank()) {
                    if (!(value.startsWith("http://") || value.startsWith("https://") ||
                            value.startsWith("ssh://") || value.startsWith("ws://"))
                    ) {
                        errors.add(
                            Edition5ConfigError(
                                key, "invalid_url",
                                "${control.name} must be an absolute URL"
                            )
                        )
                    }
                }
                if (kind == Edition5ControlKind.SECRET_REF && value.contains("=")) {
                    errors.add(
                        Edition5ConfigError(
                            key, "secret_value",
                            "${control.name} must be an environment/vault reference, not a value"
                        )
                    )
                }
            }
        }
        return errors
    }

    /**
     * Validate + apply one atomic patch. Worker thread only. The daemon
     * acknowledgement is the ONLY path to EFFECTIVE state.
     */
    fun applyOffEdt(): Edition5ConfigDecision {
        val local = validateLocal()
        if (local.isNotEmpty()) return Edition5ConfigDecision(false, null, local, emptyMap())
        val snap = snapshot
            ?: return Edition5ConfigDecision(
                false, null, emptyList(), emptyMap(),
                "contract-required: GET /native/config/effective " +
                    "(revision + per-field effective snapshot)"
            )
        val patch = synchronized(lock) { LinkedHashMap(staged) }
        if (patch.isEmpty()) return Edition5ConfigDecision(true, snap.revision, emptyList(), emptyMap())
        return try {
            val decision = source.validate(snap.revision, patch)
            if (!decision.ok) {
                synchronized(lock) {
                    for (error in decision.errors) {
                        val key = error.key
                        if (key != null) failed[key] = error.message
                    }
                }
                return decision
            }
            val appliedDecision = source.apply(snap.revision, patch)
            if (appliedDecision.ok) {
                synchronized(lock) {
                    applied.putAll(patch.mapValues { it.value ?: "" })
                    staged.clear()
                    stagedOwner.clear()
                    failed.clear()
                    snapshot = snap.copy(
                        revision = appliedDecision.revision ?: snap.revision,
                        values = applied + patch.mapValues { it.value ?: "" }
                    )
                }
            } else {
                synchronized(lock) {
                    for (error in appliedDecision.errors) {
                        val key = error.key
                        if (key != null) failed[key] = error.message
                    }
                }
            }
            appliedDecision
        } catch (e: Exception) {
            val reason = typedReason(e)
            val gap = if (e is Edition5ContractRequired || "contract-required" in reason) reason else null
            if (gap == null) {
                synchronized(lock) { failed["*"] = reason }
            }
            Edition5ConfigDecision(false, null, emptyList(), emptyMap(), gap)
        }
    }

    fun resetToApplied() {
        synchronized(lock) {
            staged.clear()
            stagedOwner.clear()
            failed.clear()
        }
    }

    fun restoreDefaults() {
        synchronized(lock) {
            staged.clear()
            stagedOwner.clear()
            failed.clear()
            for (control in Edition5Catalog.controls) {
                val key = keyOf(control)
                val defaultValue = defaultFor(control)
                if (defaultValue.isNotEmpty()) {
                    staged[key] = defaultValue
                    stagedOwner[key] = control.id
                }
            }
        }
    }

    fun close() = source.close()

    fun keyOf(control: Edition5Control): String =
        control.primaryConfigKey ?: "edition5.${control.id.lowercase()}"

    private fun controlFor(key: String): Edition5Control? =
        Edition5Catalog.controls.firstOrNull { keyOf(it) == key }

    private fun defaultFor(control: Edition5Control): String {
        val text = control.defaultText
        val choices = Edition5ControlKinds.choices(control)
        if (choices != null) {
            return choices.firstOrNull { text.lowercase().contains(it.lowercase()) } ?: ""
        }
        return when (Edition5ControlKinds.of(control)) {
            Edition5ControlKind.BOOLEAN ->
                if (text.startsWith("On", true) || text.contains("enabled", true)) "true" else ""
            else -> ""
        }
    }

    private fun typedReason(error: Exception): String {
        if (error is Edition5ContractRequired) return error.message
        val message = error.message ?: error.javaClass.simpleName
        return if ("contract" in message.lowercase()) message else "unavailable: $message"
    }
}

/**
 * The settings center surface: category tree/list + search + the complete
 * form for every control in the selected category. It is pure Swing so the
 * same component renders in the platform Configurable, the tool window and
 * the offscreen visual/host matrix.
 */
internal class Edition5SettingsPanel(
    private val controller: Edition5SettingsController,
    private val general: GeneralSettingsSurface? = null,
    initialCategory: String? = null
) : JPanel(BorderLayout()) {

    /** General settings adapter supplied by the platform Configurable. */
    interface GeneralSettingsSurface {
        val component: JComponent
        fun generalState(): String
        fun applyGeneral(): String?
    }

    private val worker: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "faktor-edition5-config").apply { isDaemon = true }
    }

    private val categoryModel = DefaultListModel<String>()
    private val categoryList = JList(categoryModel)
    private val categoryCombo = JComboBox<String>()
    private val search = JTextField(20)
    private val cards = JPanel(CardLayout())
    private val header = WrappedLabel("Settings")
    private val revisionLabel = mutedLabel("reading revision…")
    private val statusLabel = WrappedLabel("No staged changes.")
    private val cardIndex = LinkedHashMap<String, JPanel>()
    private val rows = ArrayList<Edition5ControlRow>()
    private val loading = AtomicBoolean(false)
    @Volatile
    private var idleLatch = java.util.concurrent.CountDownLatch(1)

    private var lastWidth = 0
    private var appliedRevisionLabel: String = ""

    init {
        background = panelSurface()
        header.font = sectionTitleFont()
        val categories = ArrayList<String>()
        if (general != null) categories.add(GENERAL)
        categories.addAll(Edition5Catalog.categories)
        for (category in categories) {
            categoryModel.addElement(category)
            categoryCombo.addItem(category)
            val card = buildCategory(category)
            cardIndex[category] = card
            cards.add(card, category)
        }
        categoryList.selectionMode = ListSelectionModel.SINGLE_SELECTION
        val initialIndex = initialCategory?.let { wanted ->
            (0 until categoryModel.size()).firstOrNull { categoryModel.get(it) == wanted }
        } ?: 0
        categoryList.selectedIndex = initialIndex
        categoryCombo.selectedItem = categoryModel.get(initialIndex)
        categoryList.a11y("Faktor settings categories", "Choose a configuration category.")
        categoryList.addListSelectionListener {
            if (!it.valueIsAdjusting) showCategory(categoryList.selectedValue)
        }
        categoryCombo.a11y("Faktor settings category", "Choose a configuration category.")
        categoryCombo.addActionListener { showCategory(categoryCombo.selectedItem as? String) }
        search.a11y("Search Faktor settings", "Filters controls by label, help, scope or config key.")
        search.document.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent?) = filterRows()
            override fun removeUpdate(e: DocumentEvent?) = filterRows()
            override fun changedUpdate(e: DocumentEvent?) = filterRows()
        })

        val tree = JPanel(BorderLayout(0, Spacing.S))
        tree.isOpaque = false
        tree.border = BorderFactory.createEmptyBorder(Spacing.M, Spacing.M, Spacing.M, Spacing.S)
        val treeHeader = pageColumn(gap = Spacing.XS, padding = 0)
        treeHeader.add(sectionHeader("Faktor"))
        treeHeader.add(mutedLabel("Application / daemon where allowed"))
        treeHeader.add(search)
        tree.add(treeHeader, BorderLayout.NORTH)
        val listScroll = insetScroll(categoryList)
        listScroll.minimumSize = Dimension(120, 120)
        tree.add(listScroll, BorderLayout.CENTER)

        val right = JPanel(BorderLayout())
        right.isOpaque = false
        val top = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        top.add(header)
        val meta = wrapRow(revisionLabel, statusChip("staged values are not effective", SemanticState.WARNING))
        top.add(meta)
        val comboRow = wrapRow(categoryCombo)
        comboRow.isVisible = false
        top.add(comboRow)
        right.add(top, BorderLayout.NORTH)
        val scroll = insetScroll(cards)
        scroll.border = BorderFactory.createEmptyBorder()
        scroll.verticalScrollBar.unitIncrement = Spacing.L
        right.add(scroll, BorderLayout.CENTER)
        val bottom = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        bottom.add(statusLabel)
        right.add(bottom, BorderLayout.SOUTH)

        categoryCombo.putClientProperty("faktor.narrowRow", comboRow)
        val split = JSplitPane(JSplitPane.HORIZONTAL_SPLIT, tree, right)
        split.border = BorderFactory.createEmptyBorder()
        split.dividerLocation = 190
        split.isContinuousLayout = true
        add(split, BorderLayout.CENTER)
        addComponentListener(object : ComponentAdapter() {
            override fun componentResized(e: ComponentEvent?) = adaptForWidth()
        })
        loadAsync()
    }

    fun component(): JComponent = this

    fun loadAsync() {
        if (!loading.compareAndSet(false, true)) return
        idleLatch = java.util.concurrent.CountDownLatch(1)
        revisionLabel.text = "reading revision…"
        worker.execute {
            controller.readOffEdt()
            SwingUtilities.invokeLater {
                loading.set(false)
                renderHeader()
                refreshRows()
                idleLatch.countDown()
            }
        }
    }

    /** Test/harness seam: wait for the initial read, then flush EDT. */
    fun awaitIdleForTest(timeoutMs: Long = 5_000L) {
        try {
            idleLatch.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        SwingUtilities.invokeAndWait { }
    }

    fun applyAsync(onResult: (Edition5ConfigDecision) -> Unit = {}) {
        statusLabel.text = "validating and applying…"
        worker.execute {
            val decision = controller.applyOffEdt()
            SwingUtilities.invokeLater {
                renderDecision(decision)
                refreshRows()
                onResult(decision)
            }
        }
    }

    /** Configurable apply path: caller runs this OFF the EDT. */
    fun applyBlocking(): Edition5ConfigDecision = controller.applyOffEdt()

    fun reset() {
        controller.resetToApplied()
        renderHeader()
        refreshRows()
        statusLabel.text = "Reset to the last applied values."
    }

    fun restoreDefaults() {
        controller.restoreDefaults()
        refreshRows()
        statusLabel.text = "Defaults staged; Apply persists one validated patch."
    }

    fun changedCount(): Int = controller.changedCount()

    fun visibleRowCount(): Int = rows.count { it.isVisible }

    fun categoryNames(): List<String> = (0 until categoryModel.size()).map { categoryModel.get(it) }

    fun selectCategoryForTest(category: String): Boolean {
        val index = (0 until categoryModel.size()).firstOrNull { categoryModel.get(it) == category } ?: return false
        categoryList.selectedIndex = index
        categoryCombo.selectedItem = category
        return true
    }

    fun rowsForTest(): List<Edition5ControlRow> = rows.toList()

    private fun buildCategory(category: String): JPanel {
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        if (general != null && category == GENERAL) {
            val body = pageColumn(gap = Spacing.M, padding = 0)
            body.add(general.component)
            body.add(wrappedMutedLabel(general.generalState()))
            column.add(card("General", body))
            return column
        }
        val group = LinkedHashMap<String, MutableList<Edition5Control>>()
        for (control in Edition5Catalog.byCategory(category)) {
            val family = familyOf(control)
            group.getOrPut(family) { ArrayList() }.add(control)
        }
        for ((family, controls) in group) {
            val body = pageColumn(gap = Spacing.S, padding = 0)
            for (control in controls) {
                val row = Edition5ControlRow(control, controller)
                rows.add(row)
                body.add(row)
            }
            column.add(card(family, body))
            column.add(vSpace(Spacing.L))
        }
        return column
    }

    private fun familyOf(control: Edition5Control): String {
        val name = control.name.lowercase()
        return when {
            "credential" in name || "secret" in name -> "Credentials"
            "permission" in name || "sandbox" in name || "approval" in name -> "Authority"
            "context" in name || "compaction" in name || "memory" in name -> "Context"
            "mcp" in name || "tool" in name -> "Tools"
            "skill" in name || "hook" in name || "command" in name -> "Customization"
            "budget" in name || "retry" in name || "loop" in name -> "Reliability"
            "diagnostic" in name || "runtime" in name || "updater" in name -> "Runtime"
            "routing" in name || "efficien" in name || "retrieval" in name -> "Routing"
            else -> "Configuration"
        }
    }

    private fun showCategory(category: String?) {
        if (category == null) return
        (cards.layout as CardLayout).show(cards, category)
        header.text = category
        if (general != null && category == GENERAL) {
            statusLabel.text = general.generalState()
        }
        adaptForWidth()
        filterRows()
    }

    private fun refreshRows() {
        for (row in rows) row.refresh()
        renderHeader()
        filterRows()
    }

    private fun renderHeader() {
        val snapshot = controller.snapshot
        val error = controller.loadError
        val staged = controller.changedCount()
        revisionLabel.text = when {
            error != null -> error
            snapshot == null -> "reading revision…"
            else -> "revision ${snapshot.revision}" +
                (snapshot.savedRevision?.let { " • saved $it" } ?: "") +
                (snapshot.runtimeRevision?.let { " • runtime $it" } ?: "")
        }
        revisionLabel.foreground = if (error != null) semanticForeground(SemanticState.NEGATIVE) else mutedForeground()
        val revision = snapshot?.revision?.toString() ?: "unavailable"
        if (revision != appliedRevisionLabel) {
            appliedRevisionLabel = revision
            if (staged == 0) {
                statusLabel.text = if (error == null) {
                    "Effective snapshot $revision • no staged changes."
                } else {
                    error
                }
                statusLabel.foreground =
                    if (error != null) semanticForeground(SemanticState.NEGATIVE) else mutedForeground()
            }
        }
        if (staged > 0) {
            statusLabel.text = "$staged staged change(s); nothing is effective until the daemon acknowledges Apply."
            statusLabel.foreground = semanticForeground(SemanticState.WARNING)
        }
    }

    private fun renderDecision(decision: Edition5ConfigDecision) {
        statusLabel.foreground = if (decision.ok) {
            semanticForeground(SemanticState.POSITIVE)
        } else {
            semanticForeground(SemanticState.NEGATIVE)
        }
        statusLabel.text = when {
            decision.contractGap != null -> "Not applied — ${decision.contractGap}"
            decision.ok -> "Applied after daemon acknowledgement • revision ${decision.revision ?: "?"}"
            decision.errors.isNotEmpty() -> "Not applied — " + decision.errors.joinToString("; ") {
                (it.key?.plus(": ") ?: "") + it.message
            }
            else -> "Not applied — the daemon refused the patch without a typed reason."
        }
        if (decision.ok) renderHeader()
    }

    private fun filterRows() {
        val query = search.text.trim().lowercase()
        for (row in rows) {
            val visible = query.isEmpty() ||
                row.control.name.lowercase().contains(query) ||
                row.control.id.lowercase().contains(query) ||
                row.control.behavior.lowercase().contains(query) ||
                row.control.control.lowercase().contains(query)
            row.isVisible = visible
        }
        val panel = cardIndex[categoryList.selectedValue]
        panel?.revalidate()
        panel?.repaint()
    }

    private fun adaptForWidth() {
        val width = width
        if (width == lastWidth || width <= 0) return
        lastWidth = width
        val narrow = width < 480
        val comboRow = categoryCombo.getClientProperty("faktor.narrowRow") as? JComponent
        comboRow?.isVisible = narrow
        val split = getComponent(0) as? JSplitPane ?: return
        split.getComponent(0).isVisible = !narrow
        split.dividerLocation = if (narrow) 0 else 190
        revalidate()
    }

    fun shutdown() {
        controller.close()
        worker.shutdownNow()
    }

    companion object {
        internal const val GENERAL = "General"
    }
}

/**
 * One exact control row: accessible label + kind-specific editor + the frozen
 * default/scope/timing/behavior copy + the per-field apply state. Activation
 * is a real staged edit (or a typed contract-required notice for action
 * rows); no row can claim Effective without a daemon acknowledgement.
 */
internal class Edition5ControlRow(
    val control: Edition5Control,
    private val controller: Edition5SettingsController
) : JPanel(BorderLayout(Spacing.S, Spacing.XS)) {

    private val kind = Edition5ControlKinds.of(control)
    private val stateChip = statusChip("", SemanticState.DIM)
    private val errorLabel = wrappedMutedLabel("")
    private val editor: JComponent
    private val text: JTextField?
    private val area: JTextArea?
    private val check: JCheckBox?
    private val combo: JComboBox<String>?
    private var suppress = false

    init {
        isOpaque = false
        alignmentX = Component.LEFT_ALIGNMENT
        layout = BorderLayout(Spacing.S, Spacing.XS)
        val meta = "${control.id} • ${control.scope} • ${control.applyTiming}"
        val label = FixedWrapLabel(control.name).apply {
            font = sectionTitleFont()
            foreground = textForeground()
        }
        val hint = wrappedMutedLabel(
            "Default: ${control.defaultText.ifBlank { "—" }} • $meta\n${control.behavior}"
        )
        val editorAndState = JPanel(BorderLayout(Spacing.S, Spacing.XS)).apply { isOpaque = false }
        val (component, field, textArea, checkbox, choice) = buildEditor()
        editor = component
        text = field
        area = textArea
        check = checkbox
        combo = choice
        editorAndState.add(editor, BorderLayout.CENTER)
        val stateRow = wrapRow(stateChip)
        editorAndState.add(stateRow, BorderLayout.EAST)
        val body = JPanel(BorderLayout(0, Spacing.XS)).apply {
            isOpaque = false
            add(label, BorderLayout.NORTH)
            add(editorAndState, BorderLayout.CENTER)
            add(hint, BorderLayout.SOUTH)
        }
        add(body, BorderLayout.CENTER)
        add(errorLabel, BorderLayout.SOUTH)
        editor.a11y(
            "${control.name} (${control.id})",
            "${control.behavior} Default: ${control.defaultText}. Scope: ${control.scope}. " +
                "Apply timing: ${control.applyTiming}."
        )
        label.labelFor = editor
        // A 32 px control target; compact hosts may still shrink by layout.
        editor.minimumSize = Dimension(120, 32)
        editor.preferredSize = Dimension(editor.preferredSize.width.coerceAtLeast(180), 32)
        bind()
        refresh()
    }

    private data class EditorParts(
        val component: JComponent,
        val text: JTextField?,
        val area: JTextArea?,
        val check: JCheckBox?,
        val combo: JComboBox<String>?
    )

    private fun buildEditor(): EditorParts {
        val current = controller.value(control)
        return when (kind) {
            Edition5ControlKind.BOOLEAN -> {
                val box = JCheckBox("Enabled").apply {
                    isSelected = current.equals("true", true) ||
                        control.defaultText.startsWith("On", true)
                }
                box.a11y("${control.name} toggle", control.behavior)
                EditorParts(box, null, null, box, null)
            }
            Edition5ControlKind.CHOICE -> {
                val values = Edition5ControlKinds.choices(control)
                    ?: listOf("Inherit", "Default", "Custom")
                val combo = JComboBox(values.toTypedArray()).apply {
                    isEditable = Edition5ControlKinds.choices(control) == null
                    if (current.isNotEmpty()) selectedItem = current
                }
                EditorParts(combo, null, null, null, combo)
            }
            Edition5ControlKind.MULTILINE, Edition5ControlKind.JSON -> {
                val area = JTextArea(current, 3, 28).apply {
                    lineWrap = true
                    wrapStyleWord = true
                    font = uiPanelFont()
                }
                val scroll = insetScroll(area)
                scroll.minimumSize = Dimension(160, 72)
                EditorParts(scroll, null, area, null, null)
            }
            Edition5ControlKind.SECRET_REF -> {
                val field = JPasswordField(current, 24).apply {
                    echoChar = '\u2022'
                }
                val row = JPanel(BorderLayout(Spacing.XS, 0)).apply {
                    isOpaque = false
                    add(field, BorderLayout.CENTER)
                    add(mutedLabel("environment / vault reference only"), BorderLayout.SOUTH)
                }
                EditorParts(row, field, null, null, null)
            }
            Edition5ControlKind.ACTION -> {
                val button = secondaryButton(control.control.ifBlank { "Run" })
                button.addActionListener { activationNotice() }
                EditorParts(button, null, null, null, null)
            }
            Edition5ControlKind.NUMBER -> {
                val field = JTextField(current, 12)
                EditorParts(field, field, null, null, null)
            }
            else -> {
                val field = JTextField(current, 24)
                EditorParts(field, field, null, null, null)
            }
        }
    }

    private fun bind() {
        text?.document?.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent?) = edited(text.text)
            override fun removeUpdate(e: DocumentEvent?) = edited(text.text)
            override fun changedUpdate(e: DocumentEvent?) = edited(text.text)
        })
        area?.document?.addDocumentListener(object : DocumentListener {
            override fun insertUpdate(e: DocumentEvent?) = edited(area.text)
            override fun removeUpdate(e: DocumentEvent?) = edited(area.text)
            override fun changedUpdate(e: DocumentEvent?) = edited(area.text)
        })
        check?.addActionListener { if (!suppress) edited(if (check.isSelected) "true" else "false") }
        combo?.addActionListener {
            if (!suppress) edited(combo.selectedItem?.toString().orEmpty())
        }
    }

    private fun edited(value: String) {
        controller.setValue(control, value)
        refresh()
    }

    /** The harness observes activation through this exact method. */
    fun activateForTest() {
        when (kind) {
            Edition5ControlKind.BOOLEAN -> check?.doClick()
            Edition5ControlKind.ACTION -> activationNotice()
            Edition5ControlKind.CHOICE -> {
                val combo = combo ?: return
                if (combo.itemCount > 1) {
                    combo.selectedIndex = (combo.selectedIndex + 1) % combo.itemCount
                } else {
                    edited("custom")
                }
            }
            Edition5ControlKind.MULTILINE, Edition5ControlKind.JSON -> {
                val area = area ?: return
                area.text = (area.text + " ").trim()
                if (area.text.isEmpty()) area.text = "value"
                edited(area.text)
            }
            else -> {
                val field = text ?: return
                field.text = when (kind) {
                    Edition5ControlKind.NUMBER -> "1"
                    Edition5ControlKind.URL -> "https://example.invalid/v1"
                    Edition5ControlKind.PATH -> "/tmp/faktor-${control.id.lowercase()}"
                    else -> "value-${control.id.lowercase()}"
                }
                edited(field.text)
            }
        }
    }

    fun activationNotice() {
        val missing = control.endpoints.filterNot { it.startsWith("GET /models") || it.startsWith("GET /capabilities") || it.startsWith("GET /native/providers") }
        val gap = if (missing.isEmpty()) {
            "contract-required for ${control.id}: ${control.name} needs a daemon control; no route is currently served."
        } else {
            "contract-required for ${control.id}: ${missing.joinToString("; ")}"
        }
        errorLabel.text = gap
        errorLabel.foreground = semanticForeground(SemanticState.WARNING)
        stateChip.text = Edition5FieldState.CONTRACT_REQUIRED.label
        stateChip.foreground = semanticForeground(SemanticState.WARNING)
    }

    fun refresh() {
        suppress = true
        try {
            val value = controller.value(control)
            text?.let { if (it.text != value) it.text = value }
            area?.let { if (it.text != value) it.text = value }
            check?.let {
                val selected = value.equals("true", true)
                if (it.isSelected != selected && value.isNotEmpty()) it.isSelected = selected
            }
            combo?.let {
                if (value.isNotEmpty() && !it.isEditable) {
                    for (index in 0 until it.itemCount) {
                        if (it.getItemAt(index) == value) {
                            it.selectedIndex = index
                            break
                        }
                    }
                }
            }
        } finally {
            suppress = false
        }
        val state = controller.state(control)
        stateChip.text = state.label
        stateChip.foreground = when (state) {
            Edition5FieldState.EFFECTIVE -> semanticForeground(SemanticState.POSITIVE)
            Edition5FieldState.STAGED, Edition5FieldState.SAVED_NOT_APPLIED,
            Edition5FieldState.NEXT_CALL, Edition5FieldState.NEXT_TASK,
            Edition5FieldState.NEW_SESSION -> semanticForeground(SemanticState.WARNING)
            Edition5FieldState.FAILED, Edition5FieldState.CONTRACT_REQUIRED ->
                semanticForeground(SemanticState.NEGATIVE)
            else -> mutedForeground()
        }
        val error = controller.errorFor(control)
        errorLabel.text = error.orEmpty()
        errorLabel.foreground = semanticForeground(SemanticState.NEGATIVE)
        toolTipText = "${control.id}: ${control.behavior}"
    }

    fun editorComponent(): JComponent = editor

    fun stateLabel(): String = stateChip.text

    fun setEditorTextForTest(value: String) {
        when {
            text != null -> {
                text.text = value
                edited(value)
            }
            area != null -> {
                area!!.text = value
                edited(value)
            }
            check != null -> {
                check!!.isSelected = value.equals("true", true)
                edited(if (check!!.isSelected) "true" else "false")
            }
            combo != null -> {
                combo!!.selectedItem = value
                edited(value)
            }
        }
    }
}

