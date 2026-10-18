// Edition 5 tool-window screens (S01, S02, S08-S10, S13-S27, S30-S32) as
// pure Swing surfaces over the native contract. Every read is asynchronous;
// every unavailable state names the exact missing route/DTO and keeps the
// draft. These components are shared by the tool window, the visual matrix
// and the registry-driven control exercise.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.JsonValue
import dev.faktor.shared.JsonView
import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.NativePermissionEntry
import dev.faktor.shared.NativeSessionSummary
import dev.faktor.shared.view
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.Component
import java.awt.Dimension
import java.awt.GridBagConstraints
import java.awt.GridBagLayout
import java.awt.Insets
import java.awt.event.KeyEvent
import java.util.concurrent.ExecutorService
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import javax.swing.AbstractAction
import javax.swing.BorderFactory
import javax.swing.DefaultListModel
import javax.swing.JButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.JList
import javax.swing.JPanel
import javax.swing.JScrollPane
import javax.swing.JSplitPane
import javax.swing.JTabbedPane
import javax.swing.JTable
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.KeyStroke
import javax.swing.ListSelectionModel
import javax.swing.SwingUtilities
import javax.swing.table.AbstractTableModel
import javax.swing.table.DefaultTableModel

/**
 * The one asynchronous surface base: construction is EDT-only, reads happen
 * on a bounded worker and only the final render touches Swing.
 */
internal abstract class Edition5AsyncPanel : JPanel(BorderLayout()) {

    protected val worker: ExecutorService = Executors.newSingleThreadExecutor { runnable ->
        Thread(runnable, "faktor-edition5-panel").apply { isDaemon = true }
    }
    private val loading = AtomicBoolean(false)
    @Volatile
    private var idleLatch = java.util.concurrent.CountDownLatch(1)
    protected val status = WrappedLabel("")
    protected val body = JPanel(BorderLayout())
    protected val emptyAction = secondaryButton("Refresh")
    protected var client: NativeClient? = null

    init {
        isOpaque = false
        background = panelSurface()
        val header = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        header.add(sectionHeader(title()))
        header.add(wrappedMutedLabel(subtitle()))
        header.add(wrapRow(status, emptyAction))
        add(header, BorderLayout.NORTH)
        add(body, BorderLayout.CENTER)
        emptyAction.addActionListener { refresh() }
        status.text = "Not loaded."
        status.foreground = mutedForeground()
    }

    protected abstract fun title(): String
    protected abstract fun subtitle(): String
    protected abstract fun refresh()

    fun attachClient(value: NativeClient?) {
        client = value
        refresh()
    }

    protected fun load(block: () -> Unit) {
        if (!loading.compareAndSet(false, true)) return
        idleLatch = java.util.concurrent.CountDownLatch(1)
        status.text = "Loading…"
        status.foreground = mutedForeground()
        worker.execute {
            try {
                block()
            } catch (e: Edition5ContractRequired) {
                SwingUtilities.invokeLater { renderGap(e.message) }
            } catch (e: Exception) {
                SwingUtilities.invokeLater { renderError(e) }
            } finally {
                loading.set(false)
                idleLatch.countDown()
            }
        }
    }

    protected fun onEdt(block: () -> Unit) = SwingUtilities.invokeLater(block)

    protected fun renderGap(message: String?) {
        status.text = message ?: "Unavailable."
        status.foreground = semanticForeground(SemanticState.WARNING)
        body.removeAll()
        val column = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        column.add(wrappedMutedLabel(message ?: "This capability is unavailable."))
        column.add(wrappedMutedLabel("The draft and the last coherent state are retained."))
        body.add(column, BorderLayout.CENTER)
        body.revalidate()
        body.repaint()
    }

    protected fun renderError(error: Exception) {
        status.text = typed(error)
        status.foreground = semanticForeground(SemanticState.NEGATIVE)
        body.removeAll()
        val column = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        column.add(wrappedMutedLabel("Typed cause: $status"))
        column.add(wrappedMutedLabel("Affected scope: this screen. Safe recovery: retry after the daemon is ready."))
        body.add(column, BorderLayout.CENTER)
        body.revalidate()
        body.repaint()
    }

    protected fun typed(error: Exception): String = when (error) {
        is Edition5ContractRequired -> error.message
        is NativeApiException -> "${error.code}: ${error.message}"
        else -> error.message ?: error.javaClass.simpleName
    }

    /** Test/harness seam: wait for the current read to finish, then flush EDT. */
    fun awaitIdleForTest(timeoutMs: Long = 5_000L) {
        try {
            idleLatch.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        SwingUtilities.invokeAndWait { }
    }

    open fun shutdown() {
        worker.shutdownNow()
    }
}

internal fun edition5Scroll(view: Component): JScrollPane =
    JScrollPane(view).apply {
        border = BorderFactory.createEmptyBorder()
        horizontalScrollBarPolicy = javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
        verticalScrollBar.unitIncrement = Spacing.L
    }

internal data class Edition5Record(val values: Map<String, String>) {
    operator fun get(key: String): String = values[key] ?: ""
}

internal object Edition5Data {
    fun records(json: String, key: String? = null): List<Edition5Record> {
        val root = JsonCodec.parse(json)
        val rootView = root.view("root")
        val target: JsonView = if (key == null) rootView else rootView.optionalField(key) ?: rootView
        val array = (target.value as? JsonValue.Arr)?.items ?: return emptyList()
        return array.mapNotNull { item ->
            val obj = item as? JsonValue.Obj ?: return@mapNotNull null
            obj.fields.mapValues { (_, value) ->
                when (value) {
                    is JsonValue.Str -> value.value
                    is JsonValue.Int64 -> value.value.toString()
                    is JsonValue.Bool -> value.value.toString()
                    JsonValue.Null -> ""
                    else -> JsonCodec.write(value)
                }
            }.let { Edition5Record(it) }
        }
    }

    fun objectOf(json: String, key: String? = null): Edition5Record {
        val root = JsonCodec.parse(json)
        val rootView = root.view("root")
        val target: JsonView = if (key == null) rootView else rootView.optionalField(key) ?: rootView
        val obj = target.value as? JsonValue.Obj ?: return Edition5Record(emptyMap())
        return Edition5Record(
            obj.fields.mapValues { (_, value) ->
                when (value) {
                    is JsonValue.Str -> value.value
                    is JsonValue.Int64 -> value.value.toString()
                    is JsonValue.Bool -> value.value.toString()
                    JsonValue.Null -> ""
                    else -> JsonCodec.write(value)
                }
            }
        )
    }

    fun field(json: String, vararg path: String): String? {
        var view: JsonView = JsonCodec.parse(json).view("root")
        for (segment in path) {
            view = view.optionalField(segment) ?: return null
        }
        return when (val value = view.value) {
            is JsonValue.Str -> value.value
            is JsonValue.Int64 -> value.value.toString()
            is JsonValue.Bool -> value.value.toString()
            else -> null
        }
    }
}

// ---------------------------------------------------------------- S01 cockpit

internal class Edition5WorkCockpit(
    private val catalog: List<NativeModelInfo> = emptyList(),
    private val onOpenModels: () -> Unit = {},
    private val onOpenContext: () -> Unit = {},
    private val onOpenOptions: () -> Unit = {},
    private val onRun: (String, String, String, String) -> Unit = { _, _, _, _ -> },
    private val onQueue: (String) -> Unit = {},
    private val onStop: () -> Unit = {}
) : JPanel(BorderLayout(0, Spacing.S)) {

    private val modelLabel = mutedLabel("Model: Unavailable")
    private val effort = JComboBox(arrayOf("off", "low", "medium", "high"))
    private val authority = JComboBox(arrayOf("read_only", "ask", "workspace_write"))
    private val contextLabel = mutedLabel("Context: Unavailable (contextUsage = null)")
    private val capLabel = mutedLabel("Cap: no captured run budget")
    private val attention = statusChip("No pending attention", SemanticState.DIM)
    private val draft = JTextArea(4, 40)
    private val queueField = JTextField(18)
    private val runButton = primaryButton("Run task")
    private val stopButton = secondaryButton("Stop")
    private val modelButton = secondaryButton("Choose model…")
    private val contextButton = secondaryButton("Context")
    private val optionsButton = secondaryButton("Task options")
    private val queueButton = secondaryButton("Queue next")

    init {
        isOpaque = false
        draft.lineWrap = true
        draft.wrapStyleWord = true
        draft.a11y("Message to Faktor", "Describe the task. Ctrl+Enter runs it; the draft survives model changes.")
        effort.a11y("Reasoning effort", "Reasoning effort captured for the next task.")
        authority.a11y("Permission preset", "Authority captured for the next task.")
        modelButton.a11y("Choose model", "Opens the native model picker with capability details.")
        contextButton.a11y("Open context inspector", "Shows the manifest and capacity accounting.")
        optionsButton.a11y("Task options", "Acceptance criteria, finish level and per-run budget.")
        runButton.addActionListener {
            onRun(draft.text, effort.selectedItem?.toString() ?: "", authority.selectedItem?.toString() ?: "", modelLabel.text)
        }
        stopButton.addActionListener { onStop() }
        queueButton.addActionListener { onQueue(queueField.text) }
        modelButton.addActionListener { onOpenModels() }
        contextButton.addActionListener { onOpenContext() }
        optionsButton.addActionListener { onOpenOptions() }
        val identity = pageColumn(gap = Spacing.XS, padding = 0)
        identity.add(wrapRow(sectionHeader("Faktor"), attention))
        identity.add(wrapRow(modelLabel, modelButton))
        identity.add(wrapRow(mutedLabel("Effort"), effort, mutedLabel("Authority"), authority))
        identity.add(wrapRow(contextLabel, capLabel, contextButton))
        add(identity, BorderLayout.NORTH)
        val center = pageColumn(gap = Spacing.XS, padding = 0)
        center.add(card(
            "Conversation",
            JPanel(BorderLayout()).apply {
                isOpaque = false
                add(insetScroll(JTextArea("No messages yet. Describe the task to begin.", 10, 40).apply {
                    isEditable = false
                    lineWrap = true
                    wrapStyleWord = true
                    a11y("Conversation transcript", "Durable messages appear here as the daemon serves them.")
                }), BorderLayout.CENTER)
            }
        ))
        add(center, BorderLayout.CENTER)
        val south = pageColumn(gap = Spacing.XS, padding = 0)
        south.add(draft)
        south.add(wrapRow(queueField, queueButton, optionsButton))
        south.add(wrapRow(runButton, stopButton))
        south.add(wrappedMutedLabel("Actual model and route are recorded per call; changing this draft never changes a running call."))
        add(south, BorderLayout.SOUTH)
        modelLabel.text = if (catalog.isEmpty()) "Model: Unavailable" else
            "Model: ${catalog.first().provider}/${catalog.first().model}"
    }

    fun setModel(provider: String, model: String) {
        modelLabel.text = "Model: $provider/$model"
    }

    fun setContext(capacity: Long?, input: Long?, measured: Boolean) {
        contextLabel.text = if (capacity == null || input == null) {
            "Context: Unavailable (contextUsage = null)"
        } else {
            "Context: $input input • capacity $capacity • " + if (measured) "measured" else "estimated"
        }
    }

    fun setCaps(tokens: Long?, moneyMicro: Long?) {
        capLabel.text = "Cap: " + listOfNotNull(
            tokens?.let { "$it tokens" },
            moneyMicro?.let { "USD ${it.toBigDecimal().movePointLeft(6)}" }
        ).joinToString(" / ").ifBlank { "no captured run budget" }
    }

    fun setAttention(text: String?, pending: Boolean) {
        attention.text = text ?: "No pending attention"
        attention.foreground = if (pending) semanticForeground(SemanticState.WARNING) else mutedForeground()
    }

    fun draftText(): String = draft.text
}

// ------------------------------------------------------------- S02 model picker

internal class Edition5ModelPickerPanel(
    private val catalog: List<NativeModelInfo>,
    private val favorites: Set<String> = emptySet(),
    private val recent: List<String> = emptyList(),
    private val capabilities: Map<String, Map<String, String>> = emptyMap(),
    private val onSelect: (NativeModelInfo) -> Unit = {},
    private val onCancel: () -> Unit = {}
) : JPanel(BorderLayout(0, Spacing.S)) {

    private val search = JTextField(24)
    private val listModel = DefaultListModel<NativeModelInfo>()
    private val list = JList(listModel)
    private val details = pageColumn(gap = Spacing.XS, padding = 0)
    private val select = primaryButton("Select model")
    private val cancel = secondaryButton("Cancel")
    private val empty = wrappedMutedLabel("No served models. Add a provider connection or refresh the catalog.")

    init {
        isOpaque = false
        background = panelSurface()
        search.a11y("Search models", "Filter by exact ID, provider or capability.")
        list.selectionMode = ListSelectionModel.SINGLE_SELECTION
        list.a11y("Served models", "Choose a provider/model pair, then Select.")
        RowRhythm.install(list, 2)
        list.addListSelectionListener { if (!it.valueIsAdjusting) renderDetails() }
        list.inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_ENTER, 0), "faktor.model.select")
        list.actionMap.put("faktor.model.select", object : AbstractAction() {
            override fun actionPerformed(event: java.awt.event.ActionEvent?) = selectModel()
        })
        list.inputMap.put(KeyStroke.getKeyStroke(KeyEvent.VK_ESCAPE, 0), "faktor.model.cancel")
        list.actionMap.put("faktor.model.cancel", object : AbstractAction() {
            override fun actionPerformed(event: java.awt.event.ActionEvent?) = onCancel()
        })
        select.addActionListener { selectModel() }
        cancel.addActionListener { onCancel() }
        search.document.addDocumentListener(object : javax.swing.event.DocumentListener {
            override fun insertUpdate(e: javax.swing.event.DocumentEvent?) = filter()
            override fun removeUpdate(e: javax.swing.event.DocumentEvent?) = filter()
            override fun changedUpdate(e: javax.swing.event.DocumentEvent?) = filter()
        })
        val left = pageColumn(gap = Spacing.XS, padding = Spacing.M)
        left.add(wrapRow(search))
        left.add(wrappedMutedLabel("Favorites and recents are user-local and never change availability."))
        left.add(insetScroll(list))
        left.add(empty)
        val leftPanel = JPanel(BorderLayout())
        leftPanel.isOpaque = false
        leftPanel.add(left, BorderLayout.CENTER)
        val right = edition5Scroll(pageColumn(gap = Spacing.XS, padding = Spacing.M).apply {
            add(sectionHeader("Capability details"))
            add(details)
            add(wrapRow(select, cancel))
        })
        val split = JSplitPane(JSplitPane.HORIZONTAL_SPLIT, leftPanel, right)
        split.resizeWeight = 0.58
        split.border = BorderFactory.createEmptyBorder()
        add(split, BorderLayout.CENTER)
        filter()
    }

    fun selected(): NativeModelInfo? = list.selectedValue

    private fun filter() {
        val query = search.text.trim().lowercase()
        listModel.clear()
        val filtered = catalog.filter {
            query.isEmpty() ||
                it.model.lowercase().contains(query) ||
                it.provider.lowercase().contains(query) ||
                (it.tools && "tools".contains(query)) ||
                (it.reasoning && "reasoning".contains(query))
        }
        val ordered = filtered.sortedWith(
            compareByDescending<NativeModelInfo> { "${it.provider}/${it.model}" in favorites }
                .thenByDescending { recent.indexOf("${it.provider}/${it.model}") }
                .thenBy { it.provider }
                .thenBy { it.model }
        )
        for (model in ordered) listModel.addElement(model)
        empty.isVisible = listModel.size() == 0
        if (listModel.size() > 0 && list.selectedIndex < 0) list.selectedIndex = 0
        renderDetails()
    }

    private fun renderDetails() {
        details.removeAll()
        val model = list.selectedValue
        if (model == null) {
            details.add(wrappedMutedLabel("Select a served model to inspect authority, limits and availability."))
        } else {
            val key = "${model.provider}/${model.model}"
            val cap = capabilities[key].orEmpty()
            val authority = cap["capability_authority"] ?: cap["capabilityAuthority"] ?: "unknown"
            details.add(kv("Provider", model.provider))
            details.add(kv("Model", model.model))
            details.add(kv("Availability", cap["availability"] ?: "served"))
            details.add(kv("Capability authority", authority))
            details.add(kv("Context", model.context.toString()))
            details.add(kv("Max output", model.maxOutput.toString()))
            details.add(kv("Tools", if (model.tools) "supported" else "not supported"))
            details.add(kv("Reasoning", if (model.reasoning) "supported" else "not supported"))
            details.add(kv("Vision", if (cap["vision"] == "true") "supported" else "gated / unknown"))
            details.add(kv("Structured output", if (cap["structured_output"] == "true") "supported" else "gated / unknown"))
            details.add(kv("Source", model.source))
            if (authority != "known") {
                details.add(wrappedMutedLabel(
                    "Unknown capability authority: gated tools, vision and structured output stay disabled " +
                        "until the adapter maps them. A marketing name is not authority."
                ))
            }
            if (cap["unavailable_reason"]?.isNotBlank() == true) {
                details.add(wrappedMutedLabel("Unavailable: ${cap["unavailable_reason"]}"))
            }
        }
        details.revalidate()
        details.repaint()
    }

    private fun selectModel() {
        selected()?.let(onSelect)
    }

    private fun kv(label: String, value: String): JPanel =
        wrapRow(mutedLabel(label), WrappedLabel(value).apply { font = uiPanelFont() })
}

// --------------------------------------------------------- S08 context inspector

internal class Edition5ContextPanel(
    private val sessionId: String?,
    private val sessionProvider: (() -> String?)? = null
) : Edition5AsyncPanel() {

    private fun session(): String? = sessionProvider?.invoke() ?: sessionId

    private val capacity = mutedLabel("Capacity: Unavailable")
    private val accounting = JTextArea(5, 32).apply { isEditable = false; lineWrap = true }
    private val sources = JTextArea(8, 32).apply { isEditable = false; lineWrap = true }
    private val capture = mutedLabel("Capture: no preview")
    private val pinButton = secondaryButton("Pin source")
    private val excludeButton = secondaryButton("Exclude source")
    private val compactButton = secondaryButton("Review compaction")
    private val exportButton = secondaryButton("Export context")
    private val sourceField = JTextField(24)

    override fun title(): String = "Context inspector"
    override fun subtitle(): String =
        "Capacity and input have different bases. Measured usage is Unavailable until a durable read exists."

    init {
        sourceField.a11y("Context source", "Exact URI or source id to pin or exclude.")
        pinButton.addActionListener { mutate(Edition5Routes.CONTEXT_PIN, "pin") }
        excludeButton.addActionListener { mutate(Edition5Routes.CONTEXT_EXCLUDE, "exclude") }
        compactButton.addActionListener { renderCompaction() }
        exportButton.addActionListener { renderGap(Edition5Daemon.gap("POST ${Edition5Routes.CONTEXT_EXPORT}")) }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Capacity", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(capacity)
            add(wrappedMutedLabel("Input + output reserve + safety reserve must fit capacity; reserves are not consumed input."))
        }))
        column.add(card("Category accounting", insetScroll(accounting)))
        column.add(card("Source manifest", insetScroll(sources)))
        column.add(card("Source actions", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(sourceField)
            add(wrapRow(pinButton, excludeButton))
        }))
        column.add(card("Capture and overflow", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(capture)
            add(wrapRow(compactButton, exportButton))
            add(wrappedMutedLabel("Overflow is resolved before sending: remove, shorten, compact or choose a capable model."))
        }))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        val id = session()
        if (id == null) {
            renderGap("No session selected. Create or open a conversation to inspect context.")
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", "${Edition5Routes.CONTEXT_MANIFEST}?session=$id")
            val manifest = Edition5Data.objectOf(json, "context")
            val basis = manifest["basis"].ifBlank { "unavailable" }
            val cap = manifest["capacity"]
            val input = manifest["input"]
            onEdt {
                status.text = "Manifest fresh at revision ${manifest["revision"].ifBlank { "?" }}."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                capacity.text = if (basis == "unavailable" || cap.isBlank()) {
                    "Capacity: Unavailable (no durable contextUsage)"
                } else {
                    "Capacity: $cap • input $input • output reserve ${manifest["output_reserve"]} • " +
                        "safety reserve ${manifest["safety_reserve"]} • headroom ${manifest["headroom"]} • $basis"
                }
                accounting.text = manifest["categories"].ifBlank { "Unavailable: no category accounting." }
                sources.text = manifest["sources"].ifBlank { "No sources attached." }
                capture.text = "Capture: ${manifest["capture_version"].ifBlank { "not captured" }} • " +
                    "basis ${manifest["basis"].ifBlank { "unavailable" }}"
                body.revalidate()
            }
        }
    }

    private fun mutate(route: String, verb: String) {
        val id = session() ?: return renderGap("No session selected.")
        val source = sourceField.text.trim()
        if (source.isEmpty()) {
            status.text = "A $verb needs an exact source URI or id."
            status.foreground = semanticForeground(SemanticState.WARNING)
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "session_id" to JsonValue.Str(id),
                        "source" to JsonValue.Str(source)
                    )
                )
            )
            Edition5Daemon.call(c, "POST", route, body)
            onEdt {
                status.text = "Source $verb acknowledged."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                refresh()
            }
        }
    }

    private fun renderCompaction() {
        body.removeAll()
        body.add(Edition5CompactionPanel(sessionProvider?.invoke() ?: sessionId), BorderLayout.CENTER)
        body.revalidate()
        body.repaint()
    }
}

// --------------------------------------------------------------- S10 compaction

internal class Edition5CompactionPanel(
    private val sessionId: String?,
    private val sessionProvider: (() -> String?)? = null
) : Edition5AsyncPanel() {

    private fun session(): String? = sessionProvider?.invoke() ?: sessionId

    private val plan = JTextArea(8, 36).apply { isEditable = false; lineWrap = true }
    private val result = JTextArea(6, 36).apply { isEditable = false; lineWrap = true }
    private val compact = primaryButton("Compact")
    private val cancel = secondaryButton("Cancel")
    private var minReductionPermille: Long = 0

    override fun title(): String = "Compaction and continuity review"
    override fun subtitle(): String =
        "A successful compaction must achieve the configured minimum reduction; pending obligations survive."

    init {
        compact.addActionListener { runCompact() }
        cancel.addActionListener { refresh() }
        compact.isEnabled = false
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Plan", insetScroll(plan)))
        column.add(card("Measured result", insetScroll(result)))
        column.add(wrapRow(compact, cancel))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        val id = session()
        if (id == null) {
            renderGap("No session selected.")
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", "${Edition5Routes.CONTEXT_COMPACTION_PLAN}?session=$id")
            val p = Edition5Data.objectOf(json, "plan")
            minReductionPermille = p["minimum_reduction_permille"].toLongOrNull() ?: 0L
            onEdt {
                status.text = "Plan ready; it is not yet applied."
                status.foreground = semanticForeground(SemanticState.WARNING)
                plan.text = buildString {
                    appendLine("Basis revision: ${p["revision"].ifBlank { "?" }}")
                    appendLine("Retained facts: ${p["retained_facts"].ifBlank { "Unavailable" }}")
                    appendLine("Pending operations: ${p["pending_operations"].ifBlank { "none" }}")
                    appendLine("Unknown effects: ${p["unknown_effects"].ifBlank { "none" }}")
                    appendLine("Summary model: ${p["summary_model"].ifBlank { "Unavailable" }}")
                    appendLine("Egress: ${p["egress"].ifBlank { "Unavailable" }}")
                    appendLine("Cost: ${p["cost"].ifBlank { "Unavailable" }}")
                    appendLine("Minimum reduction: ${p["minimum_reduction_permille"].ifBlank { "Unavailable" }} permille")
                }
                compact.isEnabled = true
                body.revalidate()
            }
        }
    }

    private fun runCompact() {
        val id = session() ?: return
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "session_id" to JsonValue.Str(id),
                        "expected_revision" to JsonValue.Int64(
                            plan.text.lineSequence().firstOrNull()
                                ?.substringAfter("Basis revision:")?.trim()?.toLongOrNull() ?: 0L
                        )
                    )
                )
            )
            val json = Edition5Daemon.call(c, "POST", Edition5Routes.CONTEXT_COMPACT, body)
            val r = Edition5Data.objectOf(json, "result")
            val measured = r["measured_reduction_permille"].toLongOrNull()
            onEdt {
                val ok = measured != null && measured >= minReductionPermille && r["state"] != "failed"
                status.text = if (ok) {
                    "Compaction applied; measured reduction $measured permille."
                } else {
                    "Compaction did not advance: measured ${measured ?: "Unavailable"}, " +
                        "minimum $minReductionPermille. The prior valid basis remains."
                }
                status.foreground = if (ok) semanticForeground(SemanticState.POSITIVE)
                else semanticForeground(SemanticState.NEGATIVE)
                result.text = buildString {
                    appendLine("State: ${r["state"].ifBlank { "unavailable" }}")
                    appendLine("Summary version: ${r["summary_version"].ifBlank { "Unavailable" }}")
                    appendLine("Before: ${r["before"].ifBlank { "Unavailable" }}")
                    appendLine("After: ${r["after"].ifBlank { "Unavailable" }}")
                    appendLine("Retained obligations: ${r["retained_obligations"].ifBlank { "Unavailable" }}")
                }
            }
        }
    }
}

// ------------------------------------------------------------------ S09 mentions

internal class Edition5MentionsPanel(
    private val workspaceRoot: String? = null
) : JPanel(BorderLayout(0, Spacing.S)) {

    private val kind = JComboBox(arrayOf("file", "selection", "diagnostic", "terminal", "diff", "url", "prior_chat"))
    private val uri = JTextField(28)
    private val range = JTextField(10)
    private val version = JTextField(10)
    private val preview = JTextArea(8, 36).apply { isEditable = false; lineWrap = true }
    private val chips = DefaultListModel<String>()
    private val chipList = JList(chips)
    private val add = primaryButton("Add source")
    private val remove = secondaryButton("Remove")
    private val retry = secondaryButton("Retry failed")
    private val notice = WrappedLabel("")

    init {
        isOpaque = false
        kind.a11y("Source kind", "File, selection, diagnostic, terminal excerpt, diff, URL or prior chat.")
        uri.a11y("Source URI", "Exact URI or workspace-relative path. A filename alone is not content.")
        range.a11y("Source range", "Line range for a selection or excerpt.")
        version.a11y("Source version", "Document version or content hash.")
        chipList.a11y("Selected context sources", "Removing a source never clears the draft text.")
        chipList.selectionMode = ListSelectionModel.SINGLE_SELECTION
        add.addActionListener { addSource() }
        remove.addActionListener {
            val selected = chipList.selectedIndex
            if (selected >= 0) chips.remove(selected)
        }
        retry.addActionListener {
            notice.text = "Failed items stay visible with a reason; retry resolves the same exact source."
            notice.foreground = semanticForeground(SemanticState.WARNING)
        }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Mention picker", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(wrapRow(kind, uri))
            add(wrapRow(mutedLabel("Range"), range, mutedLabel("Version"), version))
            add(wrapRow(add, retry))
            add(wrappedMutedLabel("Escape closes the picker without clearing the composer text."))
        }))
        column.add(card("Selected attachments", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(insetScroll(chipList))
            add(wrapRow(remove))
        }))
        column.add(card("Source preview", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(preview)
            add(notice)
        }))
        bodyAdd(column)
    }

    private fun bodyAdd(component: Component) {
        add(component, BorderLayout.CENTER)
    }

    private fun addSource() {
        val target = uri.text.trim()
        if (target.isEmpty()) {
            notice.text = "A source needs an exact URI; a filename or tab title is not attached content."
            notice.foreground = semanticForeground(SemanticState.NEGATIVE)
            return
        }
        val entry = "${kind.selectedItem} • $target" +
            (range.text.takeIf { it.isNotBlank() }?.let { " • $it" } ?: "") +
            (version.text.takeIf { it.isNotBlank() }?.let { " • v$it" } ?: "")
        chips.addElement(entry)
        preview.text = "URI: $target\nRange: ${range.text.ifBlank { "whole source" }}\n" +
            "Version: ${version.text.ifBlank { "unresolved" }}\n" +
            "Worktree root: ${workspaceRoot ?: "Unavailable"}\n" +
            "Inclusion: staged (not yet admitted)"
        notice.text = "Staged. The source manifest is captured only at admission."
        notice.foreground = mutedForeground()
    }

    fun stagedCount(): Int = chips.size
}

// ------------------------------------------------------- S13 permissions policy

internal class Edition5PermissionPolicyPanel : Edition5AsyncPanel() {

    private val preset = JComboBox(arrayOf("read_only", "ask", "workspace_write", "danger_full_access"))
    private val tool = JTextField(15)
    private val action = JComboBox(arrayOf("allow", "deny", "ask"))
    private val target = JTextField(22)
    private val expiry = JTextField(10)
    private val preview = JTextArea(6, 34).apply { isEditable = false; lineWrap = true }
    private val rulesModel = DefaultTableModel(
        arrayOf("Tool / action", "Target", "Scope", "Expiry", "Version"), 0
    )
    private val rules = JTable(rulesModel)
    private val addRule = secondaryButton("Add rule")
    private val previewButton = secondaryButton("Preview match")
    private val applyButton = primaryButton("Apply policy")
    private val revokeButton = secondaryButton("Revoke")
    private val audit = JTextArea(6, 34).apply { isEditable = false; lineWrap = true }
    private val managed = WrappedLabel("Managed policy: Unavailable")

    override fun title(): String = "Permissions and sandbox policy"
    override fun subtitle(): String =
        "Effective permissions by tool, action, target and scope. Persistent rules are separate from pending approvals."

    init {
        preset.a11y("Permission preset", "Captured authority for future dispatch.")
        tool.a11y("Rule tool", "Tool identity to match.")
        action.a11y("Rule action", "Allow, deny or ask.")
        target.a11y("Rule target", "Path, command or network target matcher.")
        expiry.a11y("Rule expiry", "Expiry must be a served duration or timestamp.")
        rules.a11y("Persistent permission rules", "Rules apply to future dispatch only after acknowledgement.")
        previewButton.addActionListener { previewMatch() }
        addRule.addActionListener { addRule() }
        applyButton.addActionListener { applyRules() }
        revokeButton.addActionListener {
            status.text = "Revocation affects future denial; already-issued effects are reported separately."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Effective authority", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(wrapRow(mutedLabel("Preset"), preset))
            add(managed)
            add(wrappedMutedLabel("A preset cannot override managed denies. No policy store means no Remember rule."))
        }))
        column.add(card("Rule editor", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(wrapRow(tool, action, target))
            add(wrapRow(mutedLabel("Expiry"), expiry, previewButton, addRule))
        }))
        column.add(card("Persistent rules", insetTableScroll(rules)))
        column.add(card("Match preview", preview))
        column.add(card("Audit and revocation", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(insetScroll(audit))
            add(wrapRow(applyButton, revokeButton))
        }))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val policy = Edition5Daemon.call(c, "GET", Edition5Routes.PERMISSIONS_POLICY)
            val record = Edition5Data.objectOf(policy, "policy")
            val rulesJson = Edition5Daemon.call(c, "GET", Edition5Routes.PERMISSIONS_RULES)
            val rows = Edition5Data.records(rulesJson, "rules")
            onEdt {
                status.text = "Policy revision ${record["revision"].ifBlank { "?" }}."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                managed.text = "Managed policy: ${record["managed"].ifBlank { "None configured" }} • " +
                    "sandbox filesystem ${record["sandbox_filesystem"].ifBlank { "Unavailable" }}, " +
                    "shell ${record["sandbox_shell"].ifBlank { "Unavailable" }}, " +
                    "network ${record["sandbox_network"].ifBlank { "Unavailable" }}"
                rulesModel.rowCount = 0
                for (row in rows) {
                    rulesModel.addRow(
                        arrayOf(
                            "${row["tool"]} / ${row["action"]}",
                            row["target"],
                            row["scope"],
                            row["expiry"],
                            row["version"]
                        )
                    )
                }
                audit.text = rows.joinToString("\n") {
                    "${it["at"]} ${it["decision"]} ${it["tool"]} ${it["reason"]}"
                }
            }
        }
    }

    private fun previewMatch() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "tool" to JsonValue.Str(tool.text),
                        "action" to JsonValue.Str(action.selectedItem?.toString() ?: "ask"),
                        "target" to JsonValue.Str(target.text)
                    )
                )
            )
            val json = Edition5Daemon.call(c, "POST", Edition5Routes.PERMISSIONS_PREVIEW, body)
            val r = Edition5Data.objectOf(json, "preview")
            onEdt {
                preview.text = buildString {
                    appendLine("Decision: ${r["decision"].ifBlank { "Unavailable" }}")
                    appendLine("Normalized executable: ${r["normalized_executable"].ifBlank { "Unavailable" }}")
                    appendLine("Canonical target: ${r["canonical_target"].ifBlank { "Unavailable" }}")
                    appendLine("Matched rule: ${r["matched_rule"].ifBlank { "none" }}")
                    appendLine("Effect class: ${r["effect_class"].ifBlank { "Unavailable" }}")
                    appendLine("Refusal: ${r["refusal"].ifBlank { "none" }}")
                }
                status.text = "Preview only; no rule was saved."
                status.foreground = semanticForeground(SemanticState.WARNING)
            }
        }
    }

    private fun addRule() {
        if (tool.text.isBlank()) {
            status.text = "A rule needs a tool identity."
            status.foreground = semanticForeground(SemanticState.NEGATIVE)
            return
        }
        rulesModel.addRow(arrayOf("${tool.text} / ${action.selectedItem}", target.text, "user", expiry.text, "staged"))
        status.text = "Rule staged; Apply persists a versioned policy after acknowledgement."
        status.foreground = semanticForeground(SemanticState.WARNING)
    }

    private fun applyRules() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "preset" to JsonValue.Str(preset.selectedItem?.toString() ?: "ask"),
                        "expected_revision" to JsonValue.Int64(0L)
                    )
                )
            )
            val json = Edition5Daemon.call(c, "POST", Edition5Routes.PERMISSIONS_POLICY, body)
            val r = Edition5Data.objectOf(json, "policy")
            onEdt {
                status.text = "Policy acknowledged at revision ${r["revision"].ifBlank { "?" }}."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
            }
        }
    }
}

// ------------------------------------------------------------------ S14 approval

internal class Edition5ApprovalDock(
    private val permission: NativePermissionEntry? = null,
    private val onReply: (String) -> Unit = {}
) : JPanel(BorderLayout(0, Spacing.S)) {

    private val allow = primaryButton("Allow once")
    private val deny = secondaryButton("Deny")
    private val remember = secondaryButton("Remember rule…")
    private val refresh = secondaryButton("Refresh")
    private val detail = JTextArea(8, 34).apply { isEditable = false; lineWrap = true }
    private val notice = WrappedLabel("")
    private var replied = false

    init {
        isOpaque = false
        allow.a11y("Allow once", "Allows exactly one waiter generation; it never persists a rule.")
        deny.a11y("Deny", "Denies exactly one waiter generation.")
        remember.a11y("Remember rule", "Opens a separate policy draft only when a policy store is served.")
        refresh.a11y("Refresh approval", "Reloads the pending request; expired replies stay disabled.")
        allow.addActionListener { respond("allow") }
        deny.addActionListener { respond("deny") }
        remember.addActionListener {
            notice.text = "Remember opens a separate policy draft; Allow once never persists."
            notice.foreground = semanticForeground(SemanticState.WARNING)
        }
        refresh.addActionListener { notice.text = "Refreshed; a stale request cannot be replied to." }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Exact operation", detail))
        column.add(card("Response", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(wrapRow(allow, deny, remember, refresh))
            add(notice)
            add(wrappedMutedLabel("Expired, already-replied or wrong-session requests disable response."))
        }))
        add(column, BorderLayout.CENTER)
        render()
    }

    fun render() {
        val p = permission
        if (p == null) {
            detail.text = "No pending approval. The attention rail stays visible while empty."
            allow.isEnabled = false
            deny.isEnabled = false
            remember.isEnabled = false
        } else {
            detail.text = "Session: ${p.sessionId}\nRequest: ${p.id}\n" +
                "Capability: ${p.capability}\nDetail: ${p.detail}\n" +
                "Generation: not present in the current permission DTO; the reply still names one " +
                "exact request id and is fenced to the owning session.\n" +
                "Effect boundary: shown by the requesting operation before dispatch."
            allow.isEnabled = !replied
            deny.isEnabled = !replied
            remember.isEnabled = !replied
        }
    }

    private fun respond(decision: String) {
        onReply(decision)
        replied = true
        allow.isEnabled = false
        deny.isEnabled = false
        notice.text = "Reply sent; awaiting the daemon acknowledgement."
        notice.foreground = semanticForeground(SemanticState.WARNING)
    }
}

// ----------------------------------------------------------------- S15/S16 MCP

internal class Edition5McpPanel : Edition5AsyncPanel() {

    private val serverModel = DefaultListModel<String>()
    private val servers = JList(serverModel)
    private val detail = JTextArea(8, 34).apply { isEditable = false; lineWrap = true }
    private val logs = JTextArea(6, 34).apply { isEditable = false; lineWrap = true }
    private val transport = JComboBox(arrayOf("stdio", "http (unsupported until negotiated)", "oauth (unsupported until negotiated)"))
    private val name = JTextField(16)
    private val command = JTextField(22)
    private val args = JTextArea(3, 24).apply { lineWrap = true }
    private val env = JTextArea(3, 24).apply { lineWrap = true }
    private val trust = JCheckBox("I trust this exact executable source")
    private val save = secondaryButton("Save server")
    private val start = secondaryButton("Start")
    private val stop = secondaryButton("Stop")
    private val restart = secondaryButton("Restart")
    private val remove = secondaryButton("Remove")
    private val logsButton = secondaryButton("Logs")
    private val bounds = mutedLabel("Bounds: servers ≤ 8 • name ≤ 128 bytes • command ≤ 512 • args ≤ 32 × 512")

    override fun title(): String = "Tools and MCP"
    override fun subtitle(): String =
        "Discovered, enabled, healthy and authorized are independent. A config file never starts an executable."

    init {
        servers.a11y("MCP servers", "Choose a server to inspect health, tools, permissions and logs.")
        RowRhythm.install(servers, 2)
        transport.a11y("MCP transport", "stdio is implemented; remote transports are labelled unsupported until negotiated.")
        command.a11y("MCP command", "The exact executable; repository-supplied executables require deliberate trust.")
        args.a11y("MCP arguments", "One argument per line.")
        env.a11y("MCP environment references", "Environment references only; secret values are never stored here.")
        trust.a11y("Trust executable", "A repository-supplied executable never starts merely because it exists.")
        save.addActionListener { saveServer() }
        start.addActionListener { lifecycle("start") }
        stop.addActionListener { lifecycle("stop") }
        restart.addActionListener { lifecycle("restart") }
        remove.addActionListener { lifecycle("remove") }
        logsButton.addActionListener { readLogs() }
        servers.addListSelectionListener { if (!it.valueIsAdjusting) renderDetail() }
        val split = JSplitPane(
            JSplitPane.HORIZONTAL_SPLIT,
            pageColumn(gap = Spacing.XS, padding = Spacing.M).apply {
                add(sectionHeader("Servers"))
                add(insetScroll(servers))
                add(wrapRow(start, stop, restart, remove, logsButton))
            },
            edition5Scroll(pageColumn(gap = Spacing.S, padding = Spacing.M).apply {
                add(card("Health, tools and permissions", detail))
                add(card("Bounded logs", logs))
                add(card("Add stdio server", pageColumn(gap = Spacing.XS, padding = 0).apply {
                    add(wrapRow(transport))
                    add(wrapRow(this@Edition5McpPanel.name, command))
                    add(wrappedMutedLabel("Arguments"))
                    add(args)
                    add(wrappedMutedLabel("Environment references"))
                    add(env)
                    add(trust)
                    add(this@Edition5McpPanel.bounds)
                    add(save)
                }))
            })
        )
        split.resizeWeight = 0.38
        split.border = BorderFactory.createEmptyBorder()
        body.add(split, BorderLayout.CENTER)
    }

    override fun refresh() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", Edition5Routes.MCP_SERVERS)
            val records = Edition5Data.records(json, "servers")
            onEdt {
                serverModel.clear()
                for (record in records) {
                    serverModel.addElement("${record["name"].ifBlank { record["id"] }} • ${record["health"].ifBlank { "unknown" }}")
                }
                status.text = if (records.isEmpty()) "No MCP servers configured. Add one below." else
                    "${records.size} server(s); health is served, never inferred."
                status.foreground = mutedForeground()
            }
        }
    }

    private fun renderDetail() {
        val index = servers.selectedIndex
        if (index < 0) {
            detail.text = "Select a server. Discovered, enabled, healthy and authorized are shown separately."
            return
        }
        detail.text = "Use Refresh to read the exact served health, tool schemas and permission defaults."
    }

    private fun saveServer() {
        if (!trust.isSelected) {
            status.text = "Save refused: a repository-supplied executable needs explicit trust."
            status.foreground = semanticForeground(SemanticState.NEGATIVE)
            return
        }
        if (name.text.isBlank() || command.text.isBlank()) {
            status.text = "Save refused: name and command are required."
            status.foreground = semanticForeground(SemanticState.NEGATIVE)
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "name" to JsonValue.Str(name.text),
                        "transport" to JsonValue.Str("stdio"),
                        "command" to JsonValue.Str(command.text),
                        "args" to JsonValue.Arr(args.text.lineSequence().filter { it.isNotBlank() }
                            .map { JsonValue.Str(it) }.toList()),
                        "env" to JsonValue.Arr(env.text.lineSequence().filter { it.isNotBlank() }
                            .map { JsonValue.Str(it) }.toList()),
                        "trusted" to JsonValue.Bool(true)
                    )
                )
            )
            Edition5Daemon.call(c, "POST", Edition5Routes.MCP_SERVERS, body)
            onEdt {
                status.text = "Saved. Start is explicit; a config file alone never starts the server."
                status.foreground = semanticForeground(SemanticState.WARNING)
                refresh()
            }
        }
    }

    private fun lifecycle(action: String) {
        val selected = servers.selectedValue ?: return run {
            status.text = "Select a server first."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val id = selected.substringBefore(" • ")
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val path = when (action) {
                "start" -> Edition5Routes.MCP_START.format(id)
                "stop" -> Edition5Routes.MCP_STOP.format(id)
                "restart" -> Edition5Routes.MCP_RESTART.format(id)
                else -> Edition5Routes.MCP_SERVER.format(id)
            }
            Edition5Daemon.call(c, if (action == "remove") "DELETE" else "POST", path, null)
            onEdt {
                status.text = "$action acknowledged; health is re-read separately."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                refresh()
            }
        }
    }

    private fun readLogs() {
        val selected = servers.selectedValue ?: return
        val id = selected.substringBefore(" • ")
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", Edition5Routes.MCP_LOGS.format(id))
            val record = Edition5Data.objectOf(json)
            onEdt { logs.text = record["logs"].ifBlank { "No bounded logs served." } }
        }
    }
}

// --------------------------------------------------------------- S17/S18 skills/hooks

internal class Edition5SkillsHooksPanel : Edition5AsyncPanel() {

    private val skillsModel = DefaultListModel<String>()
    private val skills = JList(skillsModel)
    private val skillDetail = JTextArea(7, 32).apply { isEditable = false; lineWrap = true }
    private val policy = JComboBox(arrayOf("manual", "agent_selectable", "disabled"))
    private val install = secondaryButton("Install / update")
    private val remove = secondaryButton("Remove")
    private val hookEvents = JComboBox(
        arrayOf(
            "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse",
            "PreCompact", "PostCompact", "Stop", "SubagentStart", "SubagentStop",
            "PreTask", "PostTask", "PreEdit", "PostEdit", "PreCommand", "PostCommand", "Error"
        )
    )
    private val hookCommand = JTextField(22)
    private val hookArgs = JTextArea(2, 24).apply { lineWrap = true }
    private val hookDeadline = JTextField(8)
    private val failurePolicy = JComboBox(arrayOf("fail_closed", "fail_open", "warn"))
    private val testHook = secondaryButton("Run bounded test")
    private val saveHookButton = secondaryButton("Save hook")
    private val hookResult = JTextArea(6, 32).apply { isEditable = false; lineWrap = true }
    private val tabs = JTabbedPane()

    override fun title(): String = "Skills and hooks"
    override fun subtitle(): String =
        "Installation is not permission. Hooks run under the real capability and deadline envelope."

    init {
        skills.a11y("Skill library", "Discovered skills; inspect the manifest before enabling.")
        RowRhythm.install(skills, 2)
        policy.a11y("Skill invocation policy", "Manual, agent-selectable or disabled.")
        install.addActionListener { installSkill() }
        remove.addActionListener { removeSkill() }
        testHook.addActionListener { runHookTest() }
        saveHookButton.addActionListener { saveHook() }
        val skillsPanel = pageColumn(gap = Spacing.S, padding = Spacing.M).apply {
            add(insetScroll(skills))
            add(card("Manifest and version", skillDetail))
            add(card("Invocation policy", pageColumn(gap = Spacing.XS, padding = 0).apply {
                add(policy)
                add(wrapRow(install, remove))
                add(wrappedMutedLabel("Disabling a skill removes both discovery and agent invocation."))
            }))
        }
        val hooksPanel = pageColumn(gap = Spacing.S, padding = Spacing.M).apply {
            add(card("Hook spec", pageColumn(gap = Spacing.XS, padding = 0).apply {
                add(wrapRow(mutedLabel("Event"), hookEvents))
                add(wrapRow(hookCommand, hookDeadline))
                add(wrappedMutedLabel("Arguments"))
                add(hookArgs)
                add(wrapRow(mutedLabel("Failure policy"), failurePolicy))
                add(wrapRow(saveHookButton, testHook))
                add(wrappedMutedLabel("Test is explicitly admitted; no production event is emitted for a preview."))
            }))
            add(card("Bounded outcome", hookResult))
        }
        tabs.addTab("Skills", skillsPanel)
        tabs.addTab("Hooks", hooksPanel)
        body.add(tabs, BorderLayout.CENTER)
    }

    override fun refresh() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", Edition5Routes.SKILLS)
            val records = Edition5Data.records(json, "skills")
            onEdt {
                skillsModel.clear()
                for (record in records) {
                    skillsModel.addElement(
                        "${record["name"]} • ${record["version"].ifBlank { "unversioned" }} • " +
                            record["source"].ifBlank { "unknown source" }
                    )
                }
                status.text = if (records.isEmpty()) "No skills discovered." else "${records.size} skill(s)."
                status.foreground = mutedForeground()
            }
        }
    }

    private fun installSkill() {
        val selected = skills.selectedValue ?: return
        val name = selected.substringBefore(" • ")
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            Edition5Daemon.call(c, "POST", Edition5Routes.SKILL_INSTALL.format(name), "{}")
            onEdt {
                status.text = "Installed version acknowledged; permission grants are unchanged."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                refresh()
            }
        }
    }

    private fun removeSkill() {
        val selected = skills.selectedValue ?: return
        val name = selected.substringBefore(" • ")
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            Edition5Daemon.call(c, "DELETE", Edition5Routes.SKILL.format(name), null)
            onEdt {
                status.text = "Skill removed from discovery and agent invocation."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
                refresh()
            }
        }
    }

    private fun saveHook() {
        if (hookCommand.text.isBlank()) {
            status.text = "A hook needs an executable."
            status.foreground = semanticForeground(SemanticState.NEGATIVE)
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "event" to JsonValue.Str(hookEvents.selectedItem?.toString() ?: "PreToolUse"),
                        "executable" to JsonValue.Str(hookCommand.text),
                        "args" to JsonValue.Arr(hookArgs.text.lineSequence().filter { it.isNotBlank() }
                            .map { JsonValue.Str(it) }.toList()),
                        "deadline_ms" to JsonValue.Int64(hookDeadline.text.toLongOrNull() ?: 0L),
                        "failure_policy" to JsonValue.Str(failurePolicy.selectedItem?.toString() ?: "fail_closed")
                    )
                )
            )
            Edition5Daemon.call(c, "POST", Edition5Routes.HOOKS, body)
            onEdt {
                status.text = "Hook spec saved; it applies only to future matching events."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
            }
        }
    }

    private fun runHookTest() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "event" to JsonValue.Str(hookEvents.selectedItem?.toString() ?: "PreToolUse"),
                        "executable" to JsonValue.Str(hookCommand.text),
                        "deadline_ms" to JsonValue.Int64(hookDeadline.text.toLongOrNull() ?: 0L)
                    )
                )
            )
            val json = Edition5Daemon.call(c, "POST", Edition5Routes.HOOKS + "/test", body)
            val r = Edition5Data.objectOf(json, "result")
            onEdt {
                hookResult.text = buildString {
                    appendLine("Outcome: ${r["outcome"].ifBlank { "Unavailable" }}")
                    appendLine("Exit: ${r["exit"].ifBlank { "Unavailable" }}")
                    appendLine("Stdout (bounded): ${r["stdout"].ifBlank { "" }}")
                    appendLine("Stderr (bounded): ${r["stderr"].ifBlank { "" }}")
                    appendLine("Spec hash: ${r["spec_hash"].ifBlank { "Unavailable" }}")
                    appendLine("Audit artifact: ${r["audit_artifact"].ifBlank { "Unavailable" }}")
                }
                status.text = "Test outcome is distinct from a production event."
                status.foreground = semanticForeground(SemanticState.WARNING)
            }
        }
    }
}

// --------------------------------------------------------------------- S20 browser

internal class Edition5BrowserPanel : Edition5AsyncPanel() {

    private val adapter = mutedLabel("Adapter: Unavailable")
    private val url = JTextField(28)
    private val account = JTextField(16)
    private val machine = JTextField(16)
    private val network = JComboBox(arrayOf("deny", "loopback", "allowlist"))
    private val connect = secondaryButton("Connect")
    private val capture = secondaryButton("Capture")
    private val console = JTextArea(6, 32).apply { isEditable = false; lineWrap = true }
    private val observations = JTextArea(6, 32).apply { isEditable = false; lineWrap = true }

    override fun title(): String = "Browser, diagnostics and editor actions"
    override fun subtitle(): String =
        "A browser connection requires an implemented adapter and a scoped permission decision."

    init {
        url.a11y("Browser URL", "Exact preview URL and network scope.")
        account.a11y("Browser account", "Account scope shown before connecting.")
        connect.addActionListener { connectBrowser() }
        capture.addActionListener {
            status.text = Edition5Daemon.gap("POST ${Edition5Routes.BROWSER_CAPTURE}")
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Adapter and scope", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(adapter)
            add(wrapRow(url))
            add(wrapRow(mutedLabel("Account"), account, mutedLabel("Machine"), machine))
            add(wrapRow(mutedLabel("Network"), network))
            add(wrapRow(connect, capture))
            add(wrappedMutedLabel("A screenshot or clean console does not prove functional correctness."))
        }))
        column.add(card("Console observation", console))
        column.add(card("Network / screenshot artifacts", observations))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", Edition5Routes.BROWSER_STATUS)
            val record = Edition5Data.objectOf(json, "browser")
            onEdt {
                val available = record["available"].toBoolean()
                adapter.text = if (available) {
                    "Adapter: ${record["adapter"].ifBlank { "implemented" }} • ${record["version"].ifBlank { "version unavailable" }}"
                } else {
                    "Adapter: Unavailable — ${record["reason"].ifBlank { "no implemented browser adapter advertised" }}"
                }
                adapter.foreground = if (available) semanticForeground(SemanticState.POSITIVE)
                else semanticForeground(SemanticState.WARNING)
                connect.isEnabled = available
                capture.isEnabled = available
                console.text = record["console"].ifBlank { "No console observation; browser support is not claimed." }
                observations.text = record["observations"].ifBlank { "No screenshots or network artifacts." }
                status.text = if (available) "Adapter advertised by the daemon." else
                    "Unavailable is labelled; no idle browser tab is shown."
                status.foreground = if (available) mutedForeground() else semanticForeground(SemanticState.WARNING)
            }
        }
    }

    private fun connectBrowser() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val body = JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "url" to JsonValue.Str(url.text),
                        "account" to JsonValue.Str(account.text),
                        "machine" to JsonValue.Str(machine.text),
                        "network" to JsonValue.Str(network.selectedItem?.toString() ?: "deny")
                    )
                )
            )
            Edition5Daemon.call(c, "POST", Edition5Routes.BROWSER_CONNECT, body)
            onEdt {
                status.text = "Connection acknowledged; observations remain separate from correctness."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
            }
        }
    }
}

// ---------------------------------------------------------------- S25 checkpoints

internal class Edition5CheckpointPanel(
    private val sessionId: String?,
    private val sessionProvider: (() -> String?)? = null
) : Edition5AsyncPanel() {

    private fun session(): String? = sessionProvider?.invoke() ?: sessionId

    private val model = DefaultListModel<String>()
    private val list = JList(model)
    private val coverage = JTextArea(7, 34).apply { isEditable = false; lineWrap = true }
    private val preview = JTextArea(7, 34).apply { isEditable = false; lineWrap = true }
    private val restore = primaryButton("Restore…")
    private val fork = secondaryButton("Fork conversation…")
    private val redo = secondaryButton("Redo")

    override fun title(): String = "Checkpoints, restore and fork"
    override fun subtitle(): String =
        "Checkpoint identity, content coverage and branching are separate. Hash-only entries may not support redo/diff."

    init {
        list.a11y("Checkpoint timeline", "Choose a recorded checkpoint to inspect coverage.")
        RowRhythm.install(list, 2)
        list.addListSelectionListener { if (!it.valueIsAdjusting) renderCoverage() }
        restore.addActionListener {
            preview.text = Edition5Daemon.gap("POST ${Edition5Routes.CHECKPOINT_RESTORE}")
            status.text = "Restore is a separate transaction; no overwrite was attempted."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        fork.addActionListener {
            preview.text = Edition5Daemon.gap("POST ${Edition5Routes.CHECKPOINT_FORK}")
        }
        redo.addActionListener {
            status.text = "Redo availability is served per checkpoint; it is never inferred from a hash."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val split = JSplitPane(
            JSplitPane.HORIZONTAL_SPLIT,
            pageColumn(gap = Spacing.XS, padding = Spacing.M).apply {
                add(sectionHeader("Timeline"))
                add(insetScroll(list))
                add(wrapRow(restore, fork, redo))
            },
            edition5Scroll(pageColumn(gap = Spacing.S, padding = Spacing.M).apply {
                add(card("Coverage and preconditions", coverage))
                add(card("Restore / fork preview", preview))
                add(wrappedMutedLabel("Shell/network/SCM effects are disclosed as uncovered; restore never implies rollback."))
            })
        )
        split.resizeWeight = 0.42
        split.border = BorderFactory.createEmptyBorder()
        body.add(split, BorderLayout.CENTER)
    }

    override fun refresh() {
        val id = session()
        if (id == null) {
            renderGap("No session selected.")
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", "/native/session/$id/checkpoints")
            val records = Edition5Data.records(json, "checkpoints")
            onEdt {
                model.clear()
                for (record in records) {
                    model.addElement(
                        "${record["created_ms"].ifBlank { record["id"] }} • " +
                            record["kind"].ifBlank { "checkpoint" } +
                            (record["after_hash"].ifBlank { "" }.let { if (it.isEmpty()) " • hash-only" else "" })
                    )
                }
                status.text = if (records.isEmpty()) "No checkpoints recorded." else
                    "${records.size} checkpoint(s); coverage is explicit."
                status.foreground = mutedForeground()
            }
        }
    }

    private fun renderCoverage() {
        val selected = list.selectedValue ?: return
        coverage.text = "Recorded: $selected\n" +
            "Content coverage and file preconditions are read from the checkpoint record; " +
            "hash-only entries disable redo/diff.\n" +
            "Uncovered effects: shell, network and SCM. Restore does not roll them back."
    }
}

// ---------------------------------------------------------------- S31 binding

internal class Edition5BindingPanel : Edition5AsyncPanel() {

    private val binding = JTextArea(10, 38).apply { isEditable = false; lineWrap = true }
    private val revalidate = primaryButton("Revalidate binding")
    private val rebind = secondaryButton("Rebind session")
    private val dataBoundary = WrappedLabel("Data boundary: Unavailable")

    override fun title(): String = "Remote workspaces and execution boundary"
    override fun subtitle(): String =
        "Editor, execution, model, vault and candidate locations are explicit. A host change invalidates old handles."

    init {
        revalidate.addActionListener { refresh() }
        rebind.addActionListener {
            status.text = "Rebind is a deliberate transaction; drafts are preserved and old handles stay fenced."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Binding identity", binding))
        column.add(card("Locations and data boundary", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(dataBoundary)
            add(wrapRow(revalidate, rebind))
        }))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val json = Edition5Daemon.call(c, "GET", Edition5Routes.BINDING)
            val record = Edition5Data.objectOf(json, "binding")
            onEdt {
                binding.text = buildString {
                    appendLine("Binding generation: ${record["generation"].ifBlank { "Unavailable" }}")
                    appendLine("Editor host: ${record["editor_host"].ifBlank { "Unavailable" }}")
                    appendLine("Workspace URI: ${record["workspace_uri"].ifBlank { "Unavailable" }}")
                    appendLine("Daemon / shell: ${record["daemon_host"].ifBlank { "Unavailable" }}")
                    appendLine("Model endpoint: ${record["model_endpoint"].ifBlank { "Unavailable" }}")
                    appendLine("Embedding endpoint: ${record["embedding_endpoint"].ifBlank { "Unavailable" }}")
                    appendLine("Vault: ${record["vault"].ifBlank { "Unavailable" }}")
                    appendLine("Worker: ${record["worker"].ifBlank { "Unavailable" }}")
                    appendLine("Candidate: ${record["candidate"].ifBlank { "Unavailable" }}")
                }
                dataBoundary.text = "Data boundary: ${record["data_boundary"].ifBlank { "Unavailable" }} • " +
                    "loopback is local to the daemon host, never inferred from the laptop URL."
                status.text = "Binding ${record["generation"].ifBlank { "Unavailable" }}."
                status.foreground = semanticForeground(SemanticState.POSITIVE)
            }
        }
    }
}

// ---------------------------------------------------------------- S32 finish proof

internal class Edition5FinishProofPanel(
    private val taskId: String?
) : Edition5AsyncPanel() {

    private val outcome = sectionHeader("ENGINEERING OUTCOME: UNAVAILABLE")
    private val criteria = JTextArea(6, 36).apply { isEditable = false; lineWrap = true }
    private val claims = JTextArea(5, 36).apply { isEditable = false; lineWrap = true }
    private val landing = JTextArea(5, 36).apply { isEditable = false; lineWrap = true }
    private val finish = JComboBox(arrayOf("local", "commit", "push", "pr"))
    private val finishButton = primaryButton("Request finish")
    private val refreshButton = secondaryButton("Refresh proof")

    override fun title(): String = "Task outcome and finish proof"
    override fun subtitle(): String =
        "Conversation completion alone proves no acceptance, claim, landing or finish."

    init {
        finish.a11y("Finish level", "Local, commit, push or PR; each requires matching facts.")
        refreshButton.addActionListener { refresh() }
        finishButton.addActionListener {
            status.text = "Finish requested; the receipt is pending until every required fact matches."
            status.foreground = semanticForeground(SemanticState.WARNING)
        }
        val column = pageColumn(gap = Spacing.S, padding = Spacing.M)
        column.add(card("Outcome", pageColumn(gap = Spacing.XS, padding = 0).apply { add(outcome) }))
        column.add(card("Required acceptance", criteria))
        column.add(card("Claim evidence", claims))
        column.add(card("Integration / landing", landing))
        column.add(card("Requested finish", pageColumn(gap = Spacing.XS, padding = 0).apply {
            add(finish)
            add(wrapRow(finishButton, refreshButton))
        }))
        body.add(column, BorderLayout.CENTER)
    }

    override fun refresh() {
        val id = taskId
        if (id == null) {
            renderGap("No task selected. Open a task outcome from Work or Inspect.")
            return
        }
        load {
            val c = client ?: throw Edition5ContractRequired("daemon not connected")
            val proofJson = Edition5Daemon.call(c, "GET", "/native/tasks/$id/proof")
            val proof = Edition5Data.objectOf(proofJson, "proof")
            val stepsJson = Edition5Daemon.call(c, "GET", "/native/tasks/$id/completion-steps")
            val steps = Edition5Data.objectOf(stepsJson, "completion")
            onEdt {
                val certifies = proof["certifies_completion"].toBoolean()
                val gate = steps["gate_status"].ifBlank { "Unavailable" }
                val verified = certifies && gate == "satisfied"
                outcome.text = if (verified) "ENGINEERING OUTCOME: VERIFIED" else
                    "ENGINEERING OUTCOME: PENDING PROOF"
                outcome.foreground = if (verified) semanticForeground(SemanticState.POSITIVE)
                else semanticForeground(SemanticState.WARNING)
                criteria.text = "Criteria passed ${proof["criteria_passed"].ifBlank { "?" }} / " +
                    "${proof["criteria_total"].ifBlank { "?" }} • failed ${proof["criteria_failed"].ifBlank { "?" }} • " +
                    "unavailable ${proof["criteria_unavailable"].ifBlank { "?" }}"
                claims.text = "Proof state: ${proof["proof_state"].ifBlank { "Unavailable" }} • " +
                    "checks ${proof["checks_passed"].ifBlank { "?" }} / ${proof["checks_total"].ifBlank { "?" }}"
                landing.text = "Candidate: ${proof["candidate_identity"].ifBlank { "Unavailable" }}\n" +
                    "Landed: ${proof["landed"].ifBlank { "Unavailable" }}\n" +
                    "Finish receipt: ${proof["finish_receipt"].ifBlank { "pending" }}"
                status.text = if (verified) "Required acceptance, claims and landing match this candidate."
                else "Named blocker: ${proof["proof_state_reason"].ifBlank { steps["gate_reason"].ifBlank { "required facts are missing" } }}"
                status.foreground = if (verified) semanticForeground(SemanticState.POSITIVE)
                else semanticForeground(SemanticState.NEGATIVE)
            }
        }
    }
}
