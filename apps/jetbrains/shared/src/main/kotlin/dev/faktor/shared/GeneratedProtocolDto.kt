// GENERATED FILE - DO NOT EDIT BY HAND.
// Source: crates/protocol/schema/faktor-protocol.schema.json (schema faktor-protocol-schema/v1)
// Regenerate: node scripts/protocol-codegen.mjs --write
// The handwritten behavior/UI code that consumes these DTOs is
// NOT generated; see crates/protocol/schema/CODEGEN.md.

package dev.faktor.shared

// The plain DTO/parser portion generated from the canonical schema. The
// JSON model (JsonValue/JsonCodec), NativeProtocolException and the
// handwritten native-wire DTOs/parsers live in NativeProtocol.kt.

// --------------------------------------------------------- DTO shapes

/** One conversation message row (parts nested). (unknown_fields: ignore) */
data class ProtocolMessage(
    val id: String,
    val role: String,
    val sessionId: String,
    val seq: Long,
    val createdMs: Long,
    val parts: List<ProtocolPart>
)

/** A typed message part: text, reasoning, tool call, tool result or summary. (unknown_fields: reject) */
sealed class ProtocolPart {
    data class Text(
        val text: String
    ) : ProtocolPart()
    data class Reasoning(
        val text: String
    ) : ProtocolPart()
    data class ToolCall(
        val toolCallId: String,
        val name: String,
        val input: JsonValue,
        val state: String
    ) : ProtocolPart()
    data class ToolResult(
        val toolCallId: String,
        val result: ProtocolToolResultBody
    ) : ProtocolPart()
    data class Summary(
        val text: String
    ) : ProtocolPart()
}

/** Bounded tool result: excerpt, exit code, artifact pointer and slice hint. (unknown_fields: reject) */
data class ProtocolToolResultBody(
    val excerpt: String,
    val exitCode: Int?,
    val artifact: String?,
    val sliceHint: String?
)

/** Additive paging metadata: applied size, next cursor, has_more, total estimate. (unknown_fields: ignore) */
data class ProtocolPageMeta(
    val size: Long,
    val cursor: Long?,
    val hasMore: Boolean,
    val totalEstimate: Long?
)

/** One bounded page of conversation messages. (unknown_fields: ignore) */
data class ProtocolMessagesPage(
    val sessionId: String,
    val messages: List<ProtocolMessage>,
    val hasMore: Boolean,
    val nextBefore: Long?,
    val page: ProtocolPageMeta
)

/** The durable session state projection. (unknown_fields: ignore) */
data class ProtocolSessionState(
    val sessionId: String,
    val state: String,
    val title: String,
    val lastEventSeq: Long,
    val agentState: ProtocolAgentStateView,
    val taskLedger: JsonValue?
)

/** The agent state machine view folded into a session state row. (unknown_fields: ignore) */
data class ProtocolAgentStateView(
    val state: String,
    val label: String,
    val active: Boolean,
    val terminal: Boolean
)

/** One durable typed binary attachment: BLAKE3 digest (64 lowercase hex), mime, optional original filename and decompressed size. Decoders ignore unknown fields (the native additive-response contract); the daemon's own request DTO rejects them. (unknown_fields: ignore) */
data class ProtocolAttachmentId(
    val digest: String,
    val mime: String,
    val filename: String?,
    val size: Long
)

/** One durable attachment REFERENCE (native additive-response contract): the surrogate reference id the retrieval routes address, plus the CAS digest and the exact presentation metadata (mime, optional filename, decompressed size). Decoders ignore unknown fields. (unknown_fields: ignore) */
data class ProtocolAttachmentRef(
    val refId: Long,
    val digest: String,
    val mime: String,
    val filename: String?,
    val size: Long
)

/** Strict request body of POST /native/session/{id}/attachments: canonical standard base64 bytes plus the declared mime. (unknown_fields: reject) */
data class ProtocolAttachmentUpload(
    val mime: String,
    val filename: String?,
    val dataBase64: String
)

/** One durable task run projection (native task-run list/state reads; additive: decoders ignore unknown fields). (unknown_fields: ignore) */
data class ProtocolTaskRun(
    val taskId: Long,
    val runId: String,
    val mode: String,
    val state: String,
    val goal: String?,
    val itemIds: List<String>,
    val model: String?
)

/** Response of a native task-run start: the durable run identity and its current state (additive: unknown fields ignored). (unknown_fields: ignore) */
data class ProtocolTaskRunStarted(
    val taskId: Long,
    val runId: String,
    val state: String
)

/** Response of a native task-run cancel (additive: unknown fields ignored). (unknown_fields: ignore) */
data class ProtocolTaskRunCancelled(
    val runId: String,
    val cancelled: Boolean
)

/** One wire work item of a native task-run start (the orchestrator's own JSON vocabulary). (unknown_fields: reject) */
data class ProtocolTaskRunWorkItem(
    val id: String,
    val kind: String,
    val summary: String?,
    val dependsOn: List<String>?,
    val acceptanceChecks: List<String>?,
    val ownership: JsonValue?,
    val requiredCapabilities: JsonValue?
)

/** Strict request body of POST /native/session/{id}/task-runs. The daemon validates every member strictly; money accepts a decimal string or a lossless JSON integer and is documented as json here. `submission_id` is the required client submission UUID of the logical start (idempotency key: a repeated key replays the original run receipt byte-for-byte). (unknown_fields: reject) */
data class ProtocolTaskRunStartRequest(
    val goal: String,
    val submissionId: String,
    val criteria: List<String>?,
    val workItems: List<ProtocolTaskRunWorkItem>?,
    val ownership: JsonValue?,
    val model: String?,
    val maxTokens: Long?,
    val maxCostMicro: JsonValue?,
    val mutationMode: String?,
    val routingMode: String?,
    val files: List<String>?,
    val attachments: List<ProtocolAttachmentId>?,
    val completionContract: JsonValue?
)

// -------------------------------------------------------- parse functions

fun parseProtocolMessage(v: JsonView): ProtocolMessage {
    return ProtocolMessage(
        id = v.field("id").string(),
        role = v.field("role").string(),
        sessionId = v.field("session_id").string(),
        seq = v.field("seq").long(),
        createdMs = v.field("created_ms").long(),
        parts = v.field("parts").array().map { parseProtocolPart(it) }
    )
}

fun parseProtocolPart(v: JsonView): ProtocolPart {
    val tag = v.field("type").string()
    return when (tag) {
        "text" -> {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("type", "text")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
            ProtocolPart.Text(
                text = v.field("text").string()
            )
        }
        "reasoning" -> {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("type", "text")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
            ProtocolPart.Reasoning(
                text = v.field("text").string()
            )
        }
        "tool_call" -> {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("type", "tool_call_id", "name", "input", "state")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
            ProtocolPart.ToolCall(
                toolCallId = v.field("tool_call_id").string(),
                name = v.field("name").string(),
                input = v.field("input").value,
                state = v.field("state").string()
            )
        }
        "tool_result" -> {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("type", "tool_call_id", "result")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
            ProtocolPart.ToolResult(
                toolCallId = v.field("tool_call_id").string(),
                result = parseProtocolToolResultBody(v.field("result"))
            )
        }
        "summary" -> {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("type", "text")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
            ProtocolPart.Summary(
                text = v.field("text").string()
            )
        }
        else -> throw NativeProtocolException(v.path, "unknown Part type " + tag)
    }
}

fun parseProtocolToolResultBody(v: JsonView): ProtocolToolResultBody {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("excerpt", "exit_code", "artifact", "slice_hint")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
    return ProtocolToolResultBody(
        excerpt = v.field("excerpt").string(),
        exitCode = v.optionalField("exit_code")?.int(),
        artifact = v.optionalField("artifact")?.string(),
        sliceHint = v.optionalField("slice_hint")?.string()
    )
}

fun parseProtocolPageMeta(v: JsonView): ProtocolPageMeta {
    return ProtocolPageMeta(
        size = v.field("size").long(),
        cursor = v.optionalField("cursor")?.long(),
        hasMore = v.field("has_more").bool(),
        totalEstimate = v.optionalField("total_estimate")?.long()
    )
}

fun parseProtocolMessagesPage(v: JsonView): ProtocolMessagesPage {
    return ProtocolMessagesPage(
        sessionId = v.field("session_id").string(),
        messages = v.field("messages").array().map { parseProtocolMessage(it) },
        hasMore = v.field("has_more").bool(),
        nextBefore = v.optionalField("next_before")?.long(),
        page = v.optionalField("page")?.let { parseProtocolPageMeta(it) } ?: ProtocolPageMeta(0L, null, false, null)
    )
}

fun parseProtocolSessionState(v: JsonView): ProtocolSessionState {
    return ProtocolSessionState(
        sessionId = v.field("session_id").string(),
        state = v.field("state").string(),
        title = v.field("title").string(),
        lastEventSeq = v.field("last_event_seq").long(),
        agentState = parseProtocolAgentStateView(v.field("agent_state")),
        taskLedger = v.optionalField("task_ledger")?.value
    )
}

fun parseProtocolAgentStateView(v: JsonView): ProtocolAgentStateView {
    return ProtocolAgentStateView(
        state = v.field("state").string(),
        label = v.field("label").string(),
        active = v.field("active").bool(),
        terminal = v.field("terminal").bool()
    )
}

fun parseProtocolAttachmentId(v: JsonView): ProtocolAttachmentId {
    return ProtocolAttachmentId(
        digest = v.field("digest").string(),
        mime = v.field("mime").string(),
        filename = v.optionalField("filename")?.string(),
        size = v.field("size").long()
    )
}

fun parseProtocolAttachmentRef(v: JsonView): ProtocolAttachmentRef {
    return ProtocolAttachmentRef(
        refId = v.field("ref_id").long(),
        digest = v.field("digest").string(),
        mime = v.field("mime").string(),
        filename = v.optionalField("filename")?.string(),
        size = v.field("size").long()
    )
}

fun parseProtocolAttachmentUpload(v: JsonView): ProtocolAttachmentUpload {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("mime", "filename", "data_base64")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
    return ProtocolAttachmentUpload(
        mime = v.field("mime").string(),
        filename = v.optionalField("filename")?.string(),
        dataBase64 = v.field("data_base64").string()
    )
}

fun parseProtocolTaskRun(v: JsonView): ProtocolTaskRun {
    return ProtocolTaskRun(
        taskId = v.field("task_id").long(),
        runId = v.field("run_id").string(),
        mode = v.field("mode").string(),
        state = v.field("state").string(),
        goal = v.optionalField("goal")?.string(),
        itemIds = v.field("item_ids").array().map { it.string() },
        model = v.optionalField("model")?.string()
    )
}

fun parseProtocolTaskRunStarted(v: JsonView): ProtocolTaskRunStarted {
    return ProtocolTaskRunStarted(
        taskId = v.field("task_id").long(),
        runId = v.field("run_id").string(),
        state = v.field("state").string()
    )
}

fun parseProtocolTaskRunCancelled(v: JsonView): ProtocolTaskRunCancelled {
    return ProtocolTaskRunCancelled(
        runId = v.field("run_id").string(),
        cancelled = v.field("cancelled").bool()
    )
}

fun parseProtocolTaskRunWorkItem(v: JsonView): ProtocolTaskRunWorkItem {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("id", "kind", "summary", "depends_on", "acceptance_checks", "ownership", "required_capabilities")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
    return ProtocolTaskRunWorkItem(
        id = v.field("id").string(),
        kind = v.field("kind").string(),
        summary = v.optionalField("summary")?.string(),
        dependsOn = v.optionalField("depends_on")?.array()?.map { it.string() },
        acceptanceChecks = v.optionalField("acceptance_checks")?.array()?.map { it.string() },
        ownership = v.optionalField("ownership")?.value,
        requiredCapabilities = v.optionalField("required_capabilities")?.value
    )
}

fun parseProtocolTaskRunStartRequest(v: JsonView): ProtocolTaskRunStartRequest {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("goal", "submission_id", "criteria", "work_items", "ownership", "model", "max_tokens", "max_cost_micro", "mutation_mode", "routing_mode", "files", "attachments", "completion_contract")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
    return ProtocolTaskRunStartRequest(
        goal = v.field("goal").string(),
        submissionId = v.field("submission_id").string(),
        criteria = v.optionalField("criteria")?.array()?.map { it.string() },
        workItems = v.optionalField("work_items")?.array()?.map { parseProtocolTaskRunWorkItem(it) },
        ownership = v.optionalField("ownership")?.value,
        model = v.optionalField("model")?.string(),
        maxTokens = v.optionalField("max_tokens")?.long(),
        maxCostMicro = v.optionalField("max_cost_micro")?.value,
        mutationMode = v.optionalField("mutation_mode")?.string(),
        routingMode = v.optionalField("routing_mode")?.string(),
        files = v.optionalField("files")?.array()?.map { it.string() },
        attachments = v.optionalField("attachments")?.array()?.map { parseProtocolAttachmentId(it) },
        completionContract = v.optionalField("completion_contract")?.value
    )
}

// ------------------------------------------------- error constants + envelope

/** The daemon's typed error envelope `{error:{code,message,retryable}}`. */
data class ProtocolErrorEnvelope(
    val code: String,
    val message: String,
    val retryable: Boolean
)

object ProtocolErrorCodes {
    val CODES: List<String> = listOf(
        "not_found",
        "conflict",
        "invalid_state",
        "permission_denied",
        "timeout",
        "cancelled",
        "store_error",
        "network_error",
        "provider_error",
        "malformed",
        "oversized",
        "rate_limited",
        "deadlock",
        "internal_error"
    )

    val HTTP_STATUS: Map<String, Int> = mapOf(
        "not_found" to 404,
        "conflict" to 409,
        "invalid_state" to 409,
        "permission_denied" to 403,
        "timeout" to 504,
        "cancelled" to 499,
        "store_error" to 500,
        "network_error" to 502,
        "provider_error" to 502,
        "malformed" to 400,
        "oversized" to 413,
        "rate_limited" to 429,
        "deadlock" to 409,
        "internal_error" to 500
    )

    val RETRYABLE: Map<String, Boolean> = mapOf(
        "not_found" to false,
        "conflict" to false,
        "invalid_state" to false,
        "permission_denied" to false,
        "timeout" to true,
        "cancelled" to false,
        "store_error" to true,
        "network_error" to true,
        "provider_error" to true,
        "malformed" to false,
        "oversized" to false,
        "rate_limited" to true,
        "deadlock" to true,
        "internal_error" to false
    )
}

/** Parse the error envelope; null for any non-conforming body (callers
 * keep their own http_error fallback). A non-boolean retryable reads as
 * false, matching the VS Code client. */
fun parseProtocolErrorEnvelope(value: JsonValue): ProtocolErrorEnvelope? {
    val root = (value as? JsonValue.Obj)?.fields?.get("error") as? JsonValue.Obj
        ?: return null
    val code = (root.fields["code"] as? JsonValue.Str)?.value ?: return null
    val message = (root.fields["message"] as? JsonValue.Str)?.value ?: return null
    val retryable = (root.fields["retryable"] as? JsonValue.Bool)?.value ?: false
    return ProtocolErrorEnvelope(code, message, retryable)
}
