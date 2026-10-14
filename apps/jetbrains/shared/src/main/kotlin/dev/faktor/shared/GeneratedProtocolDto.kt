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

/** Strict request body of POST /native/session/{id}/prompt. `submission_id` is the required client submission UUID of the logical prompt (idempotency key: a repeated key with the equal normalized body returns the original receipt byte-for-byte; a different body under the same key is a typed conflict). The daemon validates every member strictly (`deny_unknown_fields`; an unknown field or typo is a 400). (unknown_fields: reject) */
data class ProtocolSessionPromptRequest(
    val sessionId: String,
    val submissionId: String,
    val prompt: String,
    val files: List<String>?
)

/** Response of a native prompt: the accepted in-session run identity and queued flag (additive: unknown fields ignored). A submission-keyed replay returns the stored receipt byte-for-byte. (unknown_fields: ignore) */
data class ProtocolSessionPromptReceipt(
    val opId: String,
    val runId: String,
    val accepted: Boolean,
    val queued: Boolean
)

/** The Engineering Outcome Ledger's first-class aggregate metrics plus the fold's scan honesty (`scanned` / `truncated`). Money fields are decimal strings (`*Micro`) — or null when the fact is not settled: `avoidedHostedInferenceMicro` is null with `avoidedHostedInferenceMeasurement = "unavailable"` unless at least one run carried a MEASURED counterfactual (zero is only a measured zero). Monetary metrics fold only `moneyCompleteRuns`; `moneyPendingRuns` / `moneyUnavailableRuns` are excluded and counted by `moneyIncompleteExcluded`, and `verifiedChangesMoneyComplete` is the shared Successful-Run-Cost/VCSC denominator (a client must gate BOTH cost values on it, never render its empty-denominator zero). Every rate carries its served denominator so an empty denominator renders as not-measured, never a measured 0: `localOnlyRuns`, `localFinalRuns`, `localFirstRuns`, `runsWithSteps` (the Hosted-Only Rate denominator: runs with at least one model step, `hostedOnlyRuns` / `runsWithSteps`), `runs`, `checksRequired`, and `totalSpendMicro` (Rework Cost Ratio); the Frontier Escalation Rate is null unless durable execution-class authority exists and its `measured` state folds over `runs` while `partial` folds over `frontierClassedRuns` (`frontierEscalationClassedRuns` / `frontierClassedRuns`). `verifiedFirstVerificationPassChanges` / `verifiedFirstVerificationPassPpm` are the Verified First Verification Pass Rate: verified changes whose FIRST VERIFICATION attempt passed — a verification-attempt measure, never an implementation-attempt claim. `localToHostedFallback*` is the Local-to-Hosted Fallback Rate and `hostedOnly*` the Hosted-Only Rate; both classify steps by the durable `executionClass` only. `frontierEscalationMeasurement` reports `measured` | `partial` | `unmeasured`. (unknown_fields: ignore) */
data class ProtocolOutcomeMetrics(
    val runs: Long,
    val verifiedChanges: Long,
    val verifiedFirstVerificationPassChanges: Long,
    val verifiedFirstVerificationPassPpm: Long,
    val moneyCompleteRuns: Long,
    val moneyPendingRuns: Long,
    val moneyUnavailableRuns: Long,
    val moneyIncompleteExcluded: Long,
    val verifiedChangesMoneyComplete: Long,
    val totalVerifiedSpendMicro: String,
    val successfulRunCostMicro: String,
    val totalSpendMicro: String,
    val verifiedCostPerSuccessfulChangeMicro: String,
    val totalReworkSpendMicro: String,
    val reworkCostRatioPpm: Long,
    val localRuns: Long,
    val localOnlyRuns: Long,
    val localOnlyVerifiedChanges: Long,
    val localOnlyVerifiedPpm: Long,
    val localFinalRuns: Long,
    val localFinalVerifiedChanges: Long,
    val localFinalVerifiedPpm: Long,
    val localFirstRuns: Long,
    val localFirstSuccessChanges: Long,
    val localFirstSuccessPpm: Long,
    val localToHostedFallbackRuns: Long,
    val localToHostedFallbackPpm: Long,
    val hostedOnlyRuns: Long,
    val runsWithSteps: Long,
    val hostedOnlyPpm: Long,
    val avoidedHostedInferenceMicro: String?,
    val avoidedHostedInferenceMeasurement: String,
    val avoidedHostedMeasuredRuns: Long,
    val totalWallTimeMs: Long,
    val meanTimeToVerifiedChangeMs: Long,
    val humanInterventionRuns: Long,
    val humanInterventionPpm: Long,
    val rollbackRuns: Long,
    val regressionRollbackPpm: Long,
    val checksPassed: Long,
    val checksRequired: Long,
    val proofCoveragePpm: Long,
    val frontierEscalationRuns: Long,
    val frontierClassedRuns: Long,
    val frontierEscalationClassedRuns: Long,
    val frontierEscalationPpm: Long?,
    val frontierEscalationMeasurement: String,
    val scanned: Long,
    val truncated: Boolean
)

/** One provider/model step of a completed run's execution sequence. `executionClass` is the durable route-time classification (`local` | `hosted_economy` | `hosted_balanced` | `frontier` | `unknown`); `local` is CLASS-DERIVED — true only when the class is `local`. A step with no durable class authority (`unknown`: a pre-class legacy payload or an unpriced path) exposes its recorded pre-class local bit as `legacyLocal` (true only for a classless step whose stored row recorded a local execution); a class-carrying step always reports `legacyLocal` false, and the legacy bit is never promoted to a class claim. `unknown` is counted as neither local nor hosted by the metrics. `spendMicro` is a decimal string only when the run's money is settled (`complete`); null otherwise — the money is not final, never a fabricated zero. (unknown_fields: ignore) */
data class ProtocolRunOutcomeModelStep(
    val provider: String,
    val model: String,
    val local: Boolean,
    val legacyLocal: Boolean,
    val executionClass: String,
    val spendMicro: String?
)

/** One completed run's proof coverage: required checks, passed checks and the clamped coverage ppm. (unknown_fields: ignore) */
data class ProtocolRunOutcomeProof(
    val checksPassed: Long,
    val checksRequired: Long,
    val coveragePpm: Long
)

/** One completed run's changed-code scale. (unknown_fields: ignore) */
data class ProtocolRunOutcomeChangeScale(
    val files: Long,
    val addedLines: Long,
    val removedLines: Long
)

/** One completed run's wire projection in the Engineering Outcome Ledger page: identity, acceptance/integration/verification verdict, model sequence, the LATEST settled money projection (`moneyState`), wall time, interventions, proof, change scale, rollback/recovery counters and the final verification basis. Engineering facts are immutable from record time; money fields are decimal strings only when `moneyState == "complete"` and null otherwise (a pending/unavailable money fact is never a fabricated zero). `avoidedHostedMicro` is null with `avoidedHostedMeasurement = "unavailable"` when no counterfactual authority exists. `verifiedFirstVerificationPass` is the run's FIRST VERIFICATION attempt having passed (a verification-attempt measure, never a first-implementation-attempt claim). (unknown_fields: ignore) */
data class ProtocolRunOutcome(
    val runId: String,
    val sessionId: Long,
    val taskId: Long,
    val taskClass: String,
    val riskClass: String,
    val candidateSnapshot: String?,
    val finalSnapshot: String,
    val accepted: Boolean,
    val integrated: Boolean,
    val verified: Boolean,
    val verifiedFirstVerificationPass: Boolean,
    val repairAttempts: Long,
    val modelSequence: List<ProtocolRunOutcomeModelStep>,
    val moneyState: String,
    val modelSpendMicro: String?,
    val verificationSpendMicro: String?,
    val reworkSpendMicro: String?,
    val avoidedHostedMicro: String?,
    val avoidedHostedMeasurement: String,
    val wallTimeMs: Long,
    val humanInterventions: Long,
    val proof: ProtocolRunOutcomeProof,
    val changeScale: ProtocolRunOutcomeChangeScale,
    val rollbackEvents: Long,
    val recoveryEvents: Long,
    val finalBasis: String,
    val frontierEscalations: Long,
    val completedAtMs: Long
)

/** The composite oldest-key cursor of one Engineering Outcome Ledger page (`nextCursor`); absent (null) when the page was not full. (unknown_fields: ignore) */
data class ProtocolOutcomeCursor(
    val completedAtMs: Long,
    val runId: String
)

/** Response of GET /native/outcomes: the durable metrics fold plus one bounded newest-first page of completed-run projections and the composite cursor older pages continue from (null when exhausted). (unknown_fields: ignore) */
data class ProtocolOutcomesPage(
    val metrics: ProtocolOutcomeMetrics,
    val runs: List<ProtocolRunOutcome>,
    val nextCursor: ProtocolOutcomeCursor?
)

/** The value-entitlement report's governance capability picture (`governance`): `sso`/`scim` are capability vocabulary, effective only behind the `enterprise_governance` value entitlement (never an implementation). (unknown_fields: ignore) */
data class ProtocolValueGovernanceCapabilities(
    val sso: Boolean,
    val scim: Boolean
)

/** One per-entitlement value decision of the report: the entitlement tag, its typed enforcement hook, whether it is enforceable now and (only when not) the exact refusal cause. A granted decision omits `cause` (the schema field is optional). (unknown_fields: ignore) */
data class ProtocolValueEntitlementDecision(
    val entitlement: String,
    val hook: String,
    val granted: Boolean,
    val cause: String?
)

/** The caller organization's DERIVED value-entitlement report (`value_entitlements` member of GET /native/value-entitlements): the plan resolution, the plan-mapped inference-independent value tags, the effective governance capability flags and one typed decision per entitlement. The report never folds usage, credits or tokens. (unknown_fields: ignore) */
data class ProtocolValueEntitlementsReport(
    val organizationId: String,
    val planId: String?,
    val planFound: Boolean,
    val subscriptionActive: Boolean,
    val planEntitlements: List<String>,
    val governance: ProtocolValueGovernanceCapabilities,
    val decisions: List<ProtocolValueEntitlementDecision>,
    val nowMs: Long
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

fun parseProtocolSessionPromptRequest(v: JsonView): ProtocolSessionPromptRequest {
    val fields = (v.value as? JsonValue.Obj)?.fields ?: emptyMap()
    for (key in fields.keys) {
        if (key !in listOf("session_id", "submission_id", "prompt", "files")) {
            throw NativeProtocolException(v.path, "unknown field " + key)
        }
    }
    return ProtocolSessionPromptRequest(
        sessionId = v.field("session_id").string(),
        submissionId = v.field("submission_id").string(),
        prompt = v.field("prompt").string(),
        files = v.optionalField("files")?.array()?.map { it.string() }
    )
}

fun parseProtocolSessionPromptReceipt(v: JsonView): ProtocolSessionPromptReceipt {
    return ProtocolSessionPromptReceipt(
        opId = v.field("op_id").string(),
        runId = v.field("run_id").string(),
        accepted = v.field("accepted").bool(),
        queued = v.field("queued").bool()
    )
}

fun parseProtocolOutcomeMetrics(v: JsonView): ProtocolOutcomeMetrics {
    return ProtocolOutcomeMetrics(
        runs = v.field("runs").long(),
        verifiedChanges = v.field("verifiedChanges").long(),
        verifiedFirstVerificationPassChanges = v.field("verifiedFirstVerificationPassChanges").long(),
        verifiedFirstVerificationPassPpm = v.field("verifiedFirstVerificationPassPpm").long(),
        moneyCompleteRuns = v.field("moneyCompleteRuns").long(),
        moneyPendingRuns = v.field("moneyPendingRuns").long(),
        moneyUnavailableRuns = v.field("moneyUnavailableRuns").long(),
        moneyIncompleteExcluded = v.field("moneyIncompleteExcluded").long(),
        verifiedChangesMoneyComplete = v.field("verifiedChangesMoneyComplete").long(),
        totalVerifiedSpendMicro = v.field("totalVerifiedSpendMicro").string(),
        successfulRunCostMicro = v.field("successfulRunCostMicro").string(),
        totalSpendMicro = v.field("totalSpendMicro").string(),
        verifiedCostPerSuccessfulChangeMicro = v.field("verifiedCostPerSuccessfulChangeMicro").string(),
        totalReworkSpendMicro = v.field("totalReworkSpendMicro").string(),
        reworkCostRatioPpm = v.field("reworkCostRatioPpm").long(),
        localRuns = v.field("localRuns").long(),
        localOnlyRuns = v.field("localOnlyRuns").long(),
        localOnlyVerifiedChanges = v.field("localOnlyVerifiedChanges").long(),
        localOnlyVerifiedPpm = v.field("localOnlyVerifiedPpm").long(),
        localFinalRuns = v.field("localFinalRuns").long(),
        localFinalVerifiedChanges = v.field("localFinalVerifiedChanges").long(),
        localFinalVerifiedPpm = v.field("localFinalVerifiedPpm").long(),
        localFirstRuns = v.field("localFirstRuns").long(),
        localFirstSuccessChanges = v.field("localFirstSuccessChanges").long(),
        localFirstSuccessPpm = v.field("localFirstSuccessPpm").long(),
        localToHostedFallbackRuns = v.field("localToHostedFallbackRuns").long(),
        localToHostedFallbackPpm = v.field("localToHostedFallbackPpm").long(),
        hostedOnlyRuns = v.field("hostedOnlyRuns").long(),
        runsWithSteps = v.field("runsWithSteps").long(),
        hostedOnlyPpm = v.field("hostedOnlyPpm").long(),
        avoidedHostedInferenceMicro = v.optionalField("avoidedHostedInferenceMicro")?.string(),
        avoidedHostedInferenceMeasurement = v.field("avoidedHostedInferenceMeasurement").string(),
        avoidedHostedMeasuredRuns = v.field("avoidedHostedMeasuredRuns").long(),
        totalWallTimeMs = v.field("totalWallTimeMs").long(),
        meanTimeToVerifiedChangeMs = v.field("meanTimeToVerifiedChangeMs").long(),
        humanInterventionRuns = v.field("humanInterventionRuns").long(),
        humanInterventionPpm = v.field("humanInterventionPpm").long(),
        rollbackRuns = v.field("rollbackRuns").long(),
        regressionRollbackPpm = v.field("regressionRollbackPpm").long(),
        checksPassed = v.field("checksPassed").long(),
        checksRequired = v.field("checksRequired").long(),
        proofCoveragePpm = v.field("proofCoveragePpm").long(),
        frontierEscalationRuns = v.field("frontierEscalationRuns").long(),
        frontierClassedRuns = v.field("frontierClassedRuns").long(),
        frontierEscalationClassedRuns = v.field("frontierEscalationClassedRuns").long(),
        frontierEscalationPpm = v.optionalField("frontierEscalationPpm")?.long(),
        frontierEscalationMeasurement = v.field("frontierEscalationMeasurement").string(),
        scanned = v.field("scanned").long(),
        truncated = v.field("truncated").bool()
    )
}

fun parseProtocolRunOutcomeModelStep(v: JsonView): ProtocolRunOutcomeModelStep {
    return ProtocolRunOutcomeModelStep(
        provider = v.field("provider").string(),
        model = v.field("model").string(),
        local = v.field("local").bool(),
        legacyLocal = v.field("legacyLocal").bool(),
        executionClass = v.field("executionClass").string(),
        spendMicro = v.optionalField("spendMicro")?.string()
    )
}

fun parseProtocolRunOutcomeProof(v: JsonView): ProtocolRunOutcomeProof {
    return ProtocolRunOutcomeProof(
        checksPassed = v.field("checksPassed").long(),
        checksRequired = v.field("checksRequired").long(),
        coveragePpm = v.field("coveragePpm").long()
    )
}

fun parseProtocolRunOutcomeChangeScale(v: JsonView): ProtocolRunOutcomeChangeScale {
    return ProtocolRunOutcomeChangeScale(
        files = v.field("files").long(),
        addedLines = v.field("addedLines").long(),
        removedLines = v.field("removedLines").long()
    )
}

fun parseProtocolRunOutcome(v: JsonView): ProtocolRunOutcome {
    return ProtocolRunOutcome(
        runId = v.field("runId").string(),
        sessionId = v.field("sessionId").long(),
        taskId = v.field("taskId").long(),
        taskClass = v.field("taskClass").string(),
        riskClass = v.field("riskClass").string(),
        candidateSnapshot = v.optionalField("candidateSnapshot")?.string(),
        finalSnapshot = v.field("finalSnapshot").string(),
        accepted = v.field("accepted").bool(),
        integrated = v.field("integrated").bool(),
        verified = v.field("verified").bool(),
        verifiedFirstVerificationPass = v.field("verifiedFirstVerificationPass").bool(),
        repairAttempts = v.field("repairAttempts").long(),
        modelSequence = v.field("modelSequence").array().map { parseProtocolRunOutcomeModelStep(it) },
        moneyState = v.field("moneyState").string(),
        modelSpendMicro = v.optionalField("modelSpendMicro")?.string(),
        verificationSpendMicro = v.optionalField("verificationSpendMicro")?.string(),
        reworkSpendMicro = v.optionalField("reworkSpendMicro")?.string(),
        avoidedHostedMicro = v.optionalField("avoidedHostedMicro")?.string(),
        avoidedHostedMeasurement = v.field("avoidedHostedMeasurement").string(),
        wallTimeMs = v.field("wallTimeMs").long(),
        humanInterventions = v.field("humanInterventions").long(),
        proof = parseProtocolRunOutcomeProof(v.field("proof")),
        changeScale = parseProtocolRunOutcomeChangeScale(v.field("changeScale")),
        rollbackEvents = v.field("rollbackEvents").long(),
        recoveryEvents = v.field("recoveryEvents").long(),
        finalBasis = v.field("finalBasis").string(),
        frontierEscalations = v.field("frontierEscalations").long(),
        completedAtMs = v.field("completedAtMs").long()
    )
}

fun parseProtocolOutcomeCursor(v: JsonView): ProtocolOutcomeCursor {
    return ProtocolOutcomeCursor(
        completedAtMs = v.field("completedAtMs").long(),
        runId = v.field("runId").string()
    )
}

fun parseProtocolOutcomesPage(v: JsonView): ProtocolOutcomesPage {
    return ProtocolOutcomesPage(
        metrics = parseProtocolOutcomeMetrics(v.field("metrics")),
        runs = v.field("runs").array().map { parseProtocolRunOutcome(it) },
        nextCursor = v.optionalField("nextCursor")?.let { parseProtocolOutcomeCursor(it) }
    )
}

fun parseProtocolValueGovernanceCapabilities(v: JsonView): ProtocolValueGovernanceCapabilities {
    return ProtocolValueGovernanceCapabilities(
        sso = v.field("sso").bool(),
        scim = v.field("scim").bool()
    )
}

fun parseProtocolValueEntitlementDecision(v: JsonView): ProtocolValueEntitlementDecision {
    return ProtocolValueEntitlementDecision(
        entitlement = v.field("entitlement").string(),
        hook = v.field("hook").string(),
        granted = v.field("granted").bool(),
        cause = v.optionalField("cause")?.string()
    )
}

fun parseProtocolValueEntitlementsReport(v: JsonView): ProtocolValueEntitlementsReport {
    return ProtocolValueEntitlementsReport(
        organizationId = v.field("organization_id").string(),
        planId = v.optionalField("plan_id")?.string(),
        planFound = v.field("plan_found").bool(),
        subscriptionActive = v.field("subscription_active").bool(),
        planEntitlements = v.field("plan_entitlements").array().map { it.string() },
        governance = parseProtocolValueGovernanceCapabilities(v.field("governance")),
        decisions = v.field("decisions").array().map { parseProtocolValueEntitlementDecision(it) },
        nowMs = v.field("now_ms").long()
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
