// Edition 5 daemon seams. Every path here is version-negotiated under
// /native/ and every caller treats a missing route as a typed contract gap
// (never as an empty/successful state). The route list is also the exact
// "missing daemon contract" inventory reported by the control exercise.
package dev.faktor.frontend

import dev.faktor.backend.BackendEndpoint
import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.JsonValue
import dev.faktor.shared.JsonView
import dev.faktor.shared.view

/** The Edition 5 native contract paths. */
internal object Edition5Routes {
    const val CONFIG_EFFECTIVE = "/native/config/effective"
    const val CONFIG_DESCRIPTORS = "/native/config/descriptors"
    const val CONFIG_VALIDATE = "/native/config/validate"
    const val CONFIG_APPLY = "/native/config/apply"
    const val PROVIDERS = "/native/providers/connections"
    const val PROVIDER_TEST = "/native/providers/connections/%s/test"
    const val PROVIDER_ROTATE = "/native/providers/connections/%s/rotate"
    const val PROVIDER_DELETE = "/native/providers/connections/%s"
    const val MODELS_REFRESH = "/native/models/refresh"
    const val MODEL_CAPABILITIES = "/native/models/capabilities"
    const val MODEL_TUNING = "/native/models/tuning"
    const val MODEL_TUNING_PREVIEW = "/native/models/tuning/preview"
    const val PRESETS = "/native/agents/presets"
    const val CONTEXT_MANIFEST = "/native/context/manifest"
    const val CONTEXT_PIN = "/native/context/pin"
    const val CONTEXT_EXCLUDE = "/native/context/exclude"
    const val CONTEXT_CAPTURE = "/native/context/capture"
    const val CONTEXT_COMPACTION_PLAN = "/native/context/compaction/plan"
    const val CONTEXT_COMPACT = "/native/context/compaction"
    const val CONTEXT_COMPACTION_RESULT = "/native/context/compaction/result"
    const val CONTEXT_EXPORT = "/native/context/export"
    const val MEMORY = "/native/memory"
    const val INSTRUCTIONS = "/native/instructions"
    const val INSTRUCTIONS_EFFECTIVE = "/native/instructions/effective"
    const val INSTRUCTIONS_TEST = "/native/instructions/test"
    const val PERMISSIONS_POLICY = "/native/permissions/policy"
    const val PERMISSIONS_PREVIEW = "/native/permissions/preview"
    const val PERMISSIONS_RULES = "/native/permissions/rules"
    const val PERMISSIONS_AUDIT = "/native/permissions/audit"
    const val TOOLS_CATALOG = "/native/tools/catalog"
    const val TOOLS_DIAGNOSTICS = "/native/tools/diagnostics"
    const val MCP_SERVERS = "/native/mcp/servers"
    const val MCP_SERVER = "/native/mcp/servers/%s"
    const val MCP_START = "/native/mcp/servers/%s/start"
    const val MCP_STOP = "/native/mcp/servers/%s/stop"
    const val MCP_RESTART = "/native/mcp/servers/%s/restart"
    const val MCP_LOGS = "/native/mcp/servers/%s/logs"
    const val MCP_TRUST = "/native/mcp/servers/%s/trust"
    const val SKILLS = "/native/skills"
    const val SKILL = "/native/skills/%s"
    const val SKILL_INSTALL = "/native/skills/%s/install"
    const val SKILL_UPDATE = "/native/skills/%s/update"
    const val SKILL_POLICY = "/native/skills/%s/policy"
    const val COMMANDS = "/native/commands"
    const val HOOKS = "/native/hooks"
    const val HOOK = "/native/hooks/%s"
    const val HOOK_TEST = "/native/hooks/%s/test"
    const val SEARCH = "/native/messages/search"
    const val SESSION_RENAME = "/native/session/%s/rename"
    const val SESSION_PIN = "/native/session/%s/pin"
    const val SESSION_ARCHIVE = "/native/session/%s/archive"
    const val SESSION_FORK = "/native/session/%s/fork"
    const val SESSION_EXPORT = "/native/session/%s/export"
    const val CHECKPOINT_RESTORE_PREVIEW = "/native/session/%s/checkpoints/%s/restore-preview"
    const val CHECKPOINT_RESTORE = "/native/session/%s/checkpoints/%s/restore"
    const val CHECKPOINT_FORK = "/native/session/%s/checkpoints/%s/fork"
    const val PARTIAL_ACCEPT = "/native/tasks/%s/partial-accept"
    const val DIAGNOSTICS_EXPORT = "/native/diagnostics/export"
    const val SETTINGS_EXPORT = "/native/config/export"
    const val SETTINGS_IMPORT = "/native/config/import"
    const val RETENTION = "/native/retention"
    const val BINDING = "/native/workspace/binding"
    const val BROWSER_STATUS = "/native/browser/status"
    const val BROWSER_CONNECT = "/native/browser/connect"
    const val BROWSER_CAPTURE = "/native/browser/capture"
    const val BUDGET_LEDGER = "/native/budget/ledger"
    const val BUDGET_REVISION = "/native/budget/revision"
    const val EFFICIENCY_EFFECTS = "/native/efficiency/effects"

    val ALL: List<String> = listOf(
        CONFIG_EFFECTIVE, CONFIG_DESCRIPTORS, CONFIG_VALIDATE, CONFIG_APPLY,
        PROVIDERS, PROVIDER_TEST, PROVIDER_ROTATE, PROVIDER_DELETE,
        MODELS_REFRESH, MODEL_CAPABILITIES, MODEL_TUNING, MODEL_TUNING_PREVIEW,
        PRESETS, CONTEXT_MANIFEST, CONTEXT_PIN, CONTEXT_EXCLUDE, CONTEXT_CAPTURE,
        CONTEXT_COMPACTION_PLAN, CONTEXT_COMPACT, CONTEXT_COMPACTION_RESULT,
        CONTEXT_EXPORT, MEMORY, INSTRUCTIONS, INSTRUCTIONS_EFFECTIVE,
        INSTRUCTIONS_TEST, PERMISSIONS_POLICY, PERMISSIONS_PREVIEW,
        PERMISSIONS_RULES, PERMISSIONS_AUDIT, TOOLS_CATALOG, TOOLS_DIAGNOSTICS,
        MCP_SERVERS, MCP_SERVER, MCP_START, MCP_STOP, MCP_RESTART, MCP_LOGS,
        MCP_TRUST, SKILLS, SKILL, SKILL_INSTALL, SKILL_UPDATE, SKILL_POLICY,
        COMMANDS, HOOKS, HOOK, HOOK_TEST, SEARCH, SESSION_RENAME, SESSION_PIN,
        SESSION_ARCHIVE, SESSION_FORK, SESSION_EXPORT,
        CHECKPOINT_RESTORE_PREVIEW, CHECKPOINT_RESTORE, CHECKPOINT_FORK,
        PARTIAL_ACCEPT, DIAGNOSTICS_EXPORT, SETTINGS_EXPORT, SETTINGS_IMPORT,
        RETENTION, BINDING, BROWSER_STATUS, BROWSER_CONNECT, BROWSER_CAPTURE,
        BUDGET_LEDGER, BUDGET_REVISION, EFFICIENCY_EFFECTS
    )
}

/**
 * The current daemon connection for application-scoped surfaces (the
 * Configurable) without persisting any secret. The tool window publishes the
 * live connection; stop clears it.
 */
internal object FaktorConnectionRegistry {

    @Volatile
    private var connection: BackendEndpoint? = null

    @Volatile
    private var controlToken: String? = null

    fun register(next: BackendEndpoint?, token: String?) {
        connection = next
        controlToken = token
    }

    fun client(timeoutMs: Long = 5_000L): NativeClient? {
        val current = connection ?: return null
        return NativeClient.forConnection(current, controlToken, timeoutMs)
    }

    fun endpointLabel(): String = connection?.baseUrl ?: "daemon not connected"
}

/** Reads/validates/applies the one atomic configuration patch. */
internal class NativeEdition5ConfigSource(
    private val client: NativeClient
) : Edition5ConfigSource {

    override fun read(): Edition5ConfigSnapshot {
        val json = call("GET", Edition5Routes.CONFIG_EFFECTIVE, null)
        val root = JsonCodec.parse(json)
        val v = root.view("config").optionalField("config")
            ?: throw Edition5ContractRequired(
                "contract-required: GET ${Edition5Routes.CONFIG_EFFECTIVE} must serve a `config` object"
            )
        val revision = v.optionalLong("revision")
            ?: throw Edition5ContractRequired(
                "contract-required: GET ${Edition5Routes.CONFIG_EFFECTIVE} must include integer " +
                    "`revision` and per-field `fields[{key,value,source,locked,apply_timing}]`"
            )
        val values = LinkedHashMap<String, String>()
        val sources = LinkedHashMap<String, String>()
        val timing = LinkedHashMap<String, String>()
        val locked = LinkedHashSet<String>()
        val fields = v.optionalField("fields")?.arrayOrEmpty().orEmpty()
        if (fields.isNotEmpty()) {
            for (field in fields) {
                val key = field.optionalString("key") ?: continue
                values[key] = field.optionalString("value") ?: ""
                sources[key] = field.optionalString("source") ?: ""
                timing[key] = field.optionalString("apply_timing") ?: ""
                if (field.optionalBool("locked") == true) locked.add(key)
            }
        } else {
            val objectView = v.optionalField("values")
            val obj = objectView?.value as? JsonValue.Obj
            if (obj != null) {
                for ((key, value) in obj.fields) {
                    values[key] = when (value) {
                        is JsonValue.Str -> value.value
                        JsonValue.Null -> ""
                        else -> JsonCodec.write(value)
                    }
                }
            }
        }
        return Edition5ConfigSnapshot(
            revision = revision,
            savedRevision = v.optionalLong("saved_revision"),
            runtimeRevision = v.optionalLong("runtime_revision"),
            values = values,
            sources = sources,
            locked = locked,
            applyTiming = timing
        )
    }

    override fun validate(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision =
        decision("POST", Edition5Routes.CONFIG_VALIDATE, expectedRevision, patch)

    override fun apply(expectedRevision: Long, patch: Map<String, String?>): Edition5ConfigDecision =
        decision("POST", Edition5Routes.CONFIG_APPLY, expectedRevision, patch)

    private fun decision(
        method: String,
        path: String,
        expectedRevision: Long,
        patch: Map<String, String?>
    ): Edition5ConfigDecision {
        val body = patchBody(expectedRevision, patch)
        return try {
            val json = call(method, path, body)
            val v = JsonCodec.parse(json).view("config").optionalField("config")
                ?: throw Edition5ContractRequired("contract-required: response must serve a `config` object")
            val ok = v.optionalBool("ok") ?: true
            val errors = ArrayList<Edition5ConfigError>()
            for (error in v.optionalField("errors")?.arrayOrEmpty().orEmpty()) {
                errors.add(
                    Edition5ConfigError(
                        error.optionalString("key"),
                        error.optionalString("code") ?: "invalid",
                        error.optionalString("message") ?: "invalid value"
                    )
                )
            }
            val timing = LinkedHashMap<String, String>()
            val obj = v.optionalField("apply_timing")?.value as? JsonValue.Obj
            if (obj != null) {
                for ((key, value) in obj.fields) {
                    timing[key] = (value as? JsonValue.Str)?.value ?: ""
                }
            }
            Edition5ConfigDecision(
                ok = ok && errors.isEmpty(),
                revision = v.optionalLong("revision") ?: expectedRevision,
                errors = errors,
                applyTiming = timing
            )
        } catch (e: NativeApiException) {
            if (e.status == 404 || e.code == "not_found") {
                Edition5ConfigDecision(false, null, emptyList(), emptyMap(), contractGap(method, path))
            } else {
                Edition5ConfigDecision(
                    false, null,
                    listOf(Edition5ConfigError(null, e.code, e.message ?: "daemon refused")),
                    emptyMap()
                )
            }
        }
    }

    private fun patchBody(expectedRevision: Long, patch: Map<String, String?>): String {
        val fields = LinkedHashMap<String, JsonValue>()
        fields["expected_revision"] = JsonValue.Int64(expectedRevision)
        val values = LinkedHashMap<String, JsonValue>()
        for ((key, value) in patch) {
            values[key] = if (value == null) JsonValue.Null else JsonValue.Str(value)
        }
        fields["patch"] = JsonValue.Obj(values)
        fields["atomic"] = JsonValue.Bool(true)
        return JsonCodec.write(JsonValue.Obj(fields))
    }

    private fun call(method: String, path: String, body: String?): String =
        try {
            client.nativeContractJson(method, path, body)
        } catch (e: NativeApiException) {
            if (e.status == 404 || e.code == "not_found") throw Edition5ContractRequired(contractGap(method, path))
            throw e
        }

    private fun contractGap(method: String, path: String): String =
        "contract-required: $method $path (no daemon route currently served)"
}

/** Tolerant accessors for evolving Edition 5 responses (absent = null). */
internal fun JsonView.optionalLong(key: String): Long? {
    val field = optionalField(key) ?: return null
    return when (val raw = field.value) {
        is JsonValue.Int64 -> raw.value
        is JsonValue.Dbl -> raw.value.toLong()
        is JsonValue.Str -> raw.value.toLongOrNull()
        else -> null
    }
}

internal fun JsonView.optionalString(key: String): String? {
    val field = optionalField(key) ?: return null
    return (field.value as? JsonValue.Str)?.value
}

internal fun JsonView.optionalBool(key: String): Boolean? {
    val field = optionalField(key) ?: return null
    return (field.value as? JsonValue.Bool)?.value
}

internal fun JsonView.arrayOrEmpty(): List<JsonView> =
    (value as? JsonValue.Arr)?.items?.mapIndexed { index, item -> item.view("$path[$index]") }
        ?: emptyList()

/** One tolerant GET/POST used by the Edition 5 panels. */
internal object Edition5Daemon {
    fun call(client: NativeClient, method: String, path: String, body: String? = null): String =
        try {
            client.nativeContractJson(method, path, body)
        } catch (e: NativeApiException) {
            if (e.status == 404 || e.code == "not_found") {
                throw Edition5ContractRequired("contract-required: $method $path")
            }
            throw e
        }

    fun gap(route: String): String =
        "contract-required: $route (no daemon route currently served; the control keeps its draft)"
}
