// Edition 5 control and screen catalog (frozen from
// docs/specs/edition5/controls.json; see Edition5CatalogData.kt).
//
// The catalog is presentation metadata, not a source of runtime truth: a
// control's value/effective/saved state always comes from the daemon or the
// host settings store. The catalog drives the settings tree, the screen
// registry, the visual matrix and the registry-driven control exercise suite
// so all 200 rows are reachable and count-checked.
package dev.faktor.frontend

internal enum class Edition5Cluster { WORK, INSPECT, HISTORY, SETTINGS }

internal data class Edition5Control(
    val id: String,
    val categoryCode: String,
    val category: String,
    val name: String,
    val control: String,
    val screens: List<String>,
    val scope: String,
    val applyTiming: String,
    val defaultText: String,
    val behavior: String,
    val emptyState: String,
    val errorState: String,
    val disabledState: String,
    val endpoints: List<String>,
    val currentStatus: String,
    val buildPackages: List<String>,
    val acceptance: List<String>,
    val configKeys: List<String>
) {
    val primaryConfigKey: String? get() = configKeys.firstOrNull()
    val hasRuntimeEndpoint: Boolean get() = endpoints.any { it.startsWith("GET ") || it.startsWith("POST ") }
}

internal data class Edition5Screen(
    val id: String,
    val title: String,
    val cluster: Edition5Cluster,
    val controls: List<Edition5Control>
)

internal object Edition5Catalog {

    val controls: List<Edition5Control> by lazy { parse(Edition5CatalogData.RECORDS) }

    val byId: Map<String, Edition5Control> by lazy { controls.associateBy { it.id } }

    val categories: List<String> by lazy {
        controls.map { it.category }.distinct()
    }

    fun categoryLabel(code: String): String =
        controls.firstOrNull { it.categoryCode == code }?.category ?: code

    fun byCategory(category: String): List<Edition5Control> =
        controls.filter { it.category == category }

    fun byCategoryCode(code: String): List<Edition5Control> =
        controls.filter { it.categoryCode == code }

    fun forScreen(screenId: String): List<Edition5Control> =
        controls.filter { screenId in it.screens }

    val screens: List<Edition5Screen> by lazy {
        SCREEN_TITLES.map { (id, spec) ->
            Edition5Screen(id, spec.first, spec.second, forScreen(id))
        }
    }

    fun screen(id: String): Edition5Screen =
        screens.firstOrNull { it.id == id } ?: error("unknown Edition 5 screen $id")

    /** Search across labels, control copy, behavior, scope and config keys. */
    fun search(query: String, category: String? = null): List<Edition5Control> {
        val needle = query.trim().lowercase()
        val base = if (category == null) controls else byCategory(category)
        if (needle.isEmpty()) return base
        return base.filter {
            it.id.lowercase().contains(needle) ||
                it.name.lowercase().contains(needle) ||
                it.control.lowercase().contains(needle) ||
                it.scope.lowercase().contains(needle) ||
                it.applyTiming.lowercase().contains(needle) ||
                it.behavior.lowercase().contains(needle) ||
                it.configKeys.any { key -> key.lowercase().contains(needle) }
        }
    }

    private fun parse(text: String): List<Edition5Control> {
        val out = ArrayList<Edition5Control>()
        for (line in text.lineSequence()) {
            if (line.isBlank()) continue
            val parts = line.split('\t')
            if (parts.size < 18) continue
            fun at(index: Int): String = parts[index].trim()
            fun list(value: String): List<String> =
                value.split(' ', ';').map { it.trim() }.filter { it.isNotEmpty() }
            out.add(
                Edition5Control(
                    id = at(0),
                    categoryCode = at(1),
                    category = at(2),
                    name = at(3),
                    control = at(4),
                    screens = list(at(5)),
                    scope = at(6),
                    applyTiming = at(7),
                    defaultText = at(8),
                    behavior = at(9),
                    emptyState = at(10),
                    errorState = at(11),
                    disabledState = at(12),
                    endpoints = at(13).split(';').map { it.trim() }.filter { it.isNotEmpty() },
                    currentStatus = at(14),
                    buildPackages = list(at(15)),
                    acceptance = list(at(16)),
                    configKeys = list(at(17))
                )
            )
        }
        return out
    }

    private val SCREEN_TITLES: List<Pair<String, Pair<String, Edition5Cluster>>> = listOf(
        "S01" to ("Work: a complete agent cockpit" to Edition5Cluster.WORK),
        "S02" to ("Model picker and capability details" to Edition5Cluster.WORK),
        "S03" to ("Provider connection editor" to Edition5Cluster.SETTINGS),
        "S04" to ("Local model setup and runtime allocation" to Edition5Cluster.SETTINGS),
        "S05" to ("Model tuning: reasoning and generation" to Edition5Cluster.SETTINGS),
        "S06" to ("Agent presets and execution envelopes" to Edition5Cluster.SETTINGS),
        "S07" to ("Custom agent and delegation settings" to Edition5Cluster.SETTINGS),
        "S08" to ("Context inspector and capacity accounting" to Edition5Cluster.INSPECT),
        "S09" to ("Mentions, attachments and explicit context" to Edition5Cluster.WORK),
        "S10" to ("Compaction and continuity review" to Edition5Cluster.INSPECT),
        "S11" to ("Rules, instructions and effective prompt" to Edition5Cluster.SETTINGS),
        "S12" to ("Memory, learned facts and retention" to Edition5Cluster.SETTINGS),
        "S13" to ("Permissions and sandbox policy editor" to Edition5Cluster.SETTINGS),
        "S14" to ("Approval: exact operation and safe response" to Edition5Cluster.WORK),
        "S15" to ("Tools and MCP connection health" to Edition5Cluster.SETTINGS),
        "S16" to ("Add MCP server with trust and bounded config" to Edition5Cluster.SETTINGS),
        "S17" to ("Skills and command discovery" to Edition5Cluster.SETTINGS),
        "S18" to ("Hooks: lifecycle automation and testing" to Edition5Cluster.SETTINGS),
        "S19" to ("Terminal ownership and background work" to Edition5Cluster.INSPECT),
        "S20" to ("Browser, diagnostics and editor actions" to Edition5Cluster.INSPECT),
        "S21" to ("Budgets, retry policy and loop guards" to Edition5Cluster.SETTINGS),
        "S22" to ("Live run: tools, queue and attention" to Edition5Cluster.WORK),
        "S23" to ("Agents: delegation, ownership and handoff" to Edition5Cluster.INSPECT),
        "S24" to ("Native changes and review obligations" to Edition5Cluster.INSPECT),
        "S25" to ("Checkpoints, restore and fork preview" to Edition5Cluster.HISTORY),
        "S26" to ("History, search and parallel workspaces" to Edition5Cluster.HISTORY),
        "S27" to ("Usage, context and engineering outcomes" to Edition5Cluster.HISTORY),
        "S28" to ("Routing, efficiency and retrieval controls" to Edition5Cluster.SETTINGS),
        "S29" to ("Settings: scope, provenance and atomic apply" to Edition5Cluster.SETTINGS),
        "S30" to ("Diagnostics, runtime trust and recovery" to Edition5Cluster.HISTORY),
        "S31" to ("Remote workspaces and execution boundary" to Edition5Cluster.SETTINGS),
        "S32" to ("Task outcome and finish proof" to Edition5Cluster.INSPECT)
    )
}

/**
 * The widget a control row exposes. The kind decides the row's editable
 * component and how the registry-driven exercise activates it; it never
 * decides whether the value is effective (that is a served state).
 */
internal enum class Edition5ControlKind {
    TEXT, MULTILINE, NUMBER, BOOLEAN, CHOICE, SECRET_REF, PATH, URL, JSON, ACTION, DISPLAY
}

internal object Edition5ControlKinds {

    private val ACTIONS = setOf(
        "M-08", "P-10", "P-14", "C-13", "C-14", "K-03", "I-08", "Q-08", "S-04",
        "S-13", "T-12", "T-13", "T-15", "X-04", "X-05", "X-08", "X-11",
        "V-03", "V-05", "V-06", "D-03", "D-04", "D-06", "D-15", "O-03",
        "O-05", "O-12", "A-10", "A-12", "B-12", "D-13"
    )

    private val BOOLEANS = setOf(
        "P-08", "G-10", "G-13", "C-07", "K-05", "K-06", "K-07", "K-08",
        "K-09", "Q-01", "T-02", "S-02", "S-12", "X-04", "D-08", "D-09",
        "D-10", "O-06", "O-10", "B-03", "B-08", "V-11"
    )

    private val SECRETS = setOf("P-04", "T-05", "S-10", "O-07")

    private val MULTILINE = setOf(
        "A-04", "A-12", "I-01", "I-02", "I-06", "I-09", "K-10", "K-11",
        "S-05", "S-06", "S-08", "S-09", "T-05", "V-02", "D-06", "O-09"
    )

    private val NUMBERS = setOf(
        "M-09", "G-02", "G-03", "G-04", "G-05", "G-06", "G-07", "G-08",
        "B-01", "B-02", "B-06", "B-07", "B-09", "B-10", "T-10", "S-11",
        "A-08", "A-09", "P-11", "C-12", "K-01"
    )

    private val CHOICES = setOf(
        "M-01", "M-02", "M-04", "M-05", "M-06", "M-10", "M-12", "M-14",
        "P-02", "P-07", "P-09", "P-13", "G-01", "G-09", "G-11", "G-12",
        "A-01", "A-02", "A-03", "A-06", "A-11", "C-09", "C-10", "K-02",
        "K-09", "I-09", "I-10", "Q-01", "Q-02", "Q-03", "Q-04", "Q-05",
        "Q-09", "T-01", "T-04", "S-01", "S-07", "X-02", "X-06", "X-13",
        "V-08", "V-10", "D-09", "D-10", "D-11", "D-12", "O-01", "O-02",
        "O-07", "O-11", "O-13", "B-03"
    )

    private val PATHS = setOf(
        "O-04", "P-03", "P-12", "S-08", "T-03", "T-05", "X-02", "X-13"
    )

    private val URLS = setOf("P-03", "P-12", "X-07", "O-02")

    private val JSON = setOf("G-11", "A-12", "T-09", "T-10", "V-12", "O-12")

    fun of(control: Edition5Control): Edition5ControlKind {
        val id = control.id
        return when {
            id in ACTIONS -> Edition5ControlKind.ACTION
            id in SECRETS -> Edition5ControlKind.SECRET_REF
            id in MULTILINE -> Edition5ControlKind.MULTILINE
            id in BOOLEANS -> Edition5ControlKind.BOOLEAN
            id in NUMBERS -> Edition5ControlKind.NUMBER
            id in CHOICES -> Edition5ControlKind.CHOICE
            id in JSON -> Edition5ControlKind.JSON
            id in PATHS -> Edition5ControlKind.PATH
            id in URLS -> Edition5ControlKind.URL
            else -> Edition5ControlKind.TEXT
        }
    }

    /** The bounded choices the frozen contract names, or null for free text. */
    fun choices(control: Edition5Control): List<String>? = when (control.id) {
        "M-04", "K-09" -> listOf("pinned", "economy", "balanced", "auto")
        "G-01" -> listOf("off", "low", "medium", "high")
        "G-11" -> listOf("text", "json_object", "json_schema")
        "G-12" -> listOf("auto", "default", "flex", "priority", "cache")
        "P-02" -> listOf("openai_chat", "openai_responses", "anthropic", "ollama", "custom")
        "P-07" -> listOf("ollama", "llama_cpp", "lm_studio")
        "Q-01" -> listOf("read_only", "ask", "workspace_write", "danger_full_access")
        "D-09" -> listOf("compact", "standard", "comfortable")
        "D-10" -> listOf("enter", "ctrl_enter", "cmd_enter")
        "D-12" -> listOf("all", "attention_only", "off")
        "O-01" -> listOf("application", "project", "session")
        "O-11" -> listOf("local", "remote")
        "V-10" -> listOf("local", "commit", "push", "pr")
        "A-02" -> listOf("enforced", "prompt_only")
        "A-03" -> listOf("off", "plan_artifact")
        "X-13" -> listOf("local", "ssh", "wsl", "container")
        "S-07" -> listOf(
            "SessionStart", "SessionEnd", "UserPromptSubmit", "PreToolUse", "PostToolUse",
            "PreCompact", "PostCompact", "Stop", "SubagentStart", "SubagentStop",
            "PreTask", "PostTask", "PreEdit", "PostEdit", "PreCommand", "PostCommand", "Error"
        )
        else -> null
    }
}
