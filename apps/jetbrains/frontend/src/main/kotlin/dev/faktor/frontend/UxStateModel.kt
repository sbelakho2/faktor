// The shared unified-composer UX state model (identical product vocabulary
// to the VS Code client): ONE composer with one submit action (no Ask/Task
// mode switch — engineering runs ride the explicit Start task escalation),
// and cause-specific run states. The model is pure so the panel, the status
// strip and the tests all render the same mapping. Wire ids are stable;
// labels/copy are the only user-facing strings. Durable vocabulary
// (recovery, resend, contract, proof) stays behind Advanced/Verification.
package dev.faktor.frontend

/** The shared Task presentation (same exact ids as the VS Code model). */
internal enum class TaskPresentation(val wireId: String) {
    IDLE("idle"),
    STARTING("starting"),
    PLANNING("planning"),
    WORKING("working"),
    AWAITING_USER("awaiting_user"),
    CHECKING("checking"),
    DONE("done"),
    PAUSED("paused"),
    FAILED("failed")
}

/** The shared attention causes (same exact ids as the VS Code model). */
internal enum class AttentionItem(val wireId: String) {
    APPROVAL("approval"),
    FAILED_CHECK("failed_check"),
    MISSING_CREDENTIAL("missing_credential"),
    AMBIGUOUS_REQUIREMENT("ambiguous_requirement"),
    REPOSITORY_LIMITATION("repository_limitation")
}

/** Stable cause-specific state ids (shared with the VS Code product model). */
internal enum class UxStateKind(val wireId: String) {
    READY("ready"),
    STARTING("starting"),
    CREDENTIAL("credential"),
    PROVIDER_RETRYING("provider_retrying"),
    DAEMON_RECONNECTING("daemon_reconnecting"),
    NETWORK_INTERRUPTED("network_interrupted"),
    PAUSED("paused"),
    PRE_RUN_FAILED("pre_run_failed"),
    NEEDS_INPUT("needs_input"),
    CHECK_FAILED("check_failed"),
    UNRECOVERABLE("unrecoverable"),
    RUNNING("running"),
    CHECKING("checking"),
    CANCELLED("cancelled"),
    FAILED("failed")
}

/** The action a state points at. No state invents a second competing button. */
internal enum class UxActionKind {
    NONE,
    OPEN_PROVIDER_SETTINGS,
    RETRY,
    RESUME,
    OPEN_DIAGNOSTICS,
    REPLY,
    INSPECT_VERIFICATION,
    WAIT
}

internal data class UxState(
    val kind: UxStateKind,
    val label: String,
    val copy: String,
    val action: UxActionKind = UxActionKind.NONE,
    val tone: SemanticState = SemanticState.DIM
) {
    fun actionLabel(): String? = when (action) {
        UxActionKind.NONE -> null
        UxActionKind.OPEN_PROVIDER_SETTINGS -> "Open provider settings"
        UxActionKind.RETRY -> "Retry"
        UxActionKind.RESUME -> "Resume"
        UxActionKind.OPEN_DIAGNOSTICS -> "Open diagnostics"
        UxActionKind.REPLY -> "Reply"
        UxActionKind.INSPECT_VERIFICATION -> "Inspect verification"
        UxActionKind.WAIT -> "Wait"
    }
}

/**
 * Everything the cause-specific mapper may see. Each flag is a concrete
 * observed cause, never a presentation mode: the mapper only chooses the
 * most actionable cause in a fixed precedence.
 */
internal data class UxSignals(
    val credentialFailure: Boolean = false,
    val unrecoverable: Boolean = false,
    val daemonReconnecting: Boolean = false,
    val networkInterrupted: Boolean = false,
    val paused: Boolean = false,
    val preRunFailed: Boolean = false,
    val providerRetrying: Boolean = false,
    val needsInput: Boolean = false,
    val checkFailed: Boolean = false,
    val starting: Boolean = false,
    val checking: Boolean = false,
    val running: Boolean = false,
    val cancelled: Boolean = false,
    val failed: Boolean = false
)

internal object UxStates {
    val READY = UxState(
        UxStateKind.READY,
        "Ready",
        "Describe a task or ask a question.",
        UxActionKind.NONE,
        SemanticState.DIM
    )

    val STARTING = UxState(
        UxStateKind.STARTING,
        "Starting the run",
        "Your captured draft is being admitted once.",
        UxActionKind.WAIT,
        SemanticState.DIM
    )

    val CREDENTIAL = UxState(
        UxStateKind.CREDENTIAL,
        "Credential required",
        "Add or refresh the provider credential in Settings.",
        UxActionKind.OPEN_PROVIDER_SETTINGS,
        SemanticState.NEGATIVE
    )

    val PROVIDER_RETRYING = UxState(
        UxStateKind.PROVIDER_RETRYING,
        "Provider retrying",
        "The provider did not answer. Faktor retries the same request.",
        UxActionKind.WAIT,
        SemanticState.WARNING
    )

    val DAEMON_RECONNECTING = UxState(
        UxStateKind.DAEMON_RECONNECTING,
        "Reconnecting to Faktor",
        "The service stopped. Faktor reconnects when it returns.",
        UxActionKind.OPEN_DIAGNOSTICS,
        SemanticState.WARNING
    )

    val NETWORK_INTERRUPTED = UxState(
        UxStateKind.NETWORK_INTERRUPTED,
        "Connection interrupted",
        "Your place in the conversation is kept.",
        UxActionKind.RESUME,
        SemanticState.WARNING
    )

    val PAUSED = UxState(
        UxStateKind.PAUSED,
        "Live updates paused",
        "The next event was not read. Nothing is skipped.",
        UxActionKind.RESUME,
        SemanticState.WARNING
    )

    val PRE_RUN_FAILED = UxState(
        UxStateKind.PRE_RUN_FAILED,
        "Couldn't start the run",
        "No work ran. Retry sends the same draft.",
        UxActionKind.RETRY,
        SemanticState.NEGATIVE
    )

    val NEEDS_INPUT = UxState(
        UxStateKind.NEEDS_INPUT,
        "Needs your input",
        "The run is waiting for a decision or clarification.",
        UxActionKind.REPLY,
        SemanticState.WARNING
    )

    val CHECK_FAILED = UxState(
        UxStateKind.CHECK_FAILED,
        "Check failed",
        "A required check failed. Review the exact result.",
        UxActionKind.INSPECT_VERIFICATION,
        SemanticState.NEGATIVE
    )

    val UNRECOVERABLE = UxState(
        UxStateKind.UNRECOVERABLE,
        "Faktor couldn't continue",
        "The cause is recorded in diagnostics.",
        UxActionKind.OPEN_DIAGNOSTICS,
        SemanticState.NEGATIVE
    )

    val RUNNING = UxState(
        UxStateKind.RUNNING,
        "Running",
        "The candidate is separate from your workspace.",
        UxActionKind.NONE,
        SemanticState.DIM
    )

    val CHECKING = UxState(
        UxStateKind.CHECKING,
        "Checking the change",
        "Required results remain attached to this candidate.",
        UxActionKind.INSPECT_VERIFICATION,
        SemanticState.DIM
    )

    val CANCELLED = UxState(
        UxStateKind.CANCELLED,
        "Run cancelled",
        "Cancellation is a terminal run outcome, not verification success.",
        UxActionKind.NONE,
        SemanticState.WARNING
    )

    val FAILED = UxState(
        UxStateKind.FAILED,
        "Run failed",
        "The result and reason remain available for inspection.",
        UxActionKind.INSPECT_VERIFICATION,
        SemanticState.NEGATIVE
    )
}

/**
 * Cause-specific precedence. A credential/daemon/network/pause cause is
 * actionable and outranks run progress; then a failed start; then the
 * provider's own transient retry; then operator input and failed checks;
 * only then the ordinary run lifecycle. "Run needs recovery" and "resend"
 * never appear: every state names its own cause and one action.
 */
internal fun uxStateFor(signals: UxSignals): UxState = when {
    signals.credentialFailure -> UxStates.CREDENTIAL
    signals.unrecoverable -> UxStates.UNRECOVERABLE
    signals.daemonReconnecting -> UxStates.DAEMON_RECONNECTING
    signals.networkInterrupted -> UxStates.NETWORK_INTERRUPTED
    signals.paused -> UxStates.PAUSED
    signals.preRunFailed -> UxStates.PRE_RUN_FAILED
    signals.providerRetrying -> UxStates.PROVIDER_RETRYING
    signals.needsInput -> UxStates.NEEDS_INPUT
    signals.checkFailed -> UxStates.CHECK_FAILED
    signals.starting -> UxStates.STARTING
    signals.checking -> UxStates.CHECKING
    signals.running -> UxStates.RUNNING
    signals.cancelled -> UxStates.CANCELLED
    signals.failed -> UxStates.FAILED
    else -> UxStates.READY
}

/**
 * The shared coarse presentation of the same signals. The cause-specific
 * [UxState] refines HOW a state is presented; this is the stable shared
 * shape both IDEs expose (Idle/Starting/Planning/Working/AwaitingUser/
 * Checking/Done/Paused/Failed).
 */
internal fun taskPresentationFor(signals: UxSignals, done: Boolean = false): TaskPresentation = when {
    done -> TaskPresentation.DONE
    signals.starting -> TaskPresentation.STARTING
    signals.needsInput -> TaskPresentation.AWAITING_USER
    signals.checking -> TaskPresentation.CHECKING
    signals.running -> TaskPresentation.WORKING
    signals.paused || signals.networkInterrupted || signals.daemonReconnecting ->
        TaskPresentation.PAUSED
    signals.credentialFailure || signals.unrecoverable || signals.preRunFailed ||
        signals.providerRetrying || signals.checkFailed || signals.failed ->
        TaskPresentation.FAILED
    else -> TaskPresentation.IDLE
}

/**
 * Classify a bounded attention list into the shared item vocabulary. Unknown
 * free-form blockers still count as an approval-shaped operator decision;
 * only the five stable ids are ever exposed.
 */
internal fun attentionItemsFor(
    model: TaskTreeModel,
    max: Int = 3
): List<AttentionItem> {
    val out = ArrayList<AttentionItem>()
    fun add(item: AttentionItem) {
        if (!out.contains(item)) out.add(item)
    }
    if (model.taskBlockers.isNotEmpty() || model.blockers.isNotEmpty()) {
        val text = (model.taskBlockers + model.blockers.map { it.reason })
            .joinToString(" ")
            .lowercase()
        when {
            text.contains("credential") || text.contains("auth") ||
                text.contains("api key") -> add(AttentionItem.MISSING_CREDENTIAL)
            text.contains("ambiguous") || text.contains("clarif") ->
                add(AttentionItem.AMBIGUOUS_REQUIREMENT)
            text.contains("repository") || text.contains("workspace") ->
                add(AttentionItem.REPOSITORY_LIMITATION)
            else -> add(AttentionItem.APPROVAL)
        }
    }
    if (model.verification.failedChecks > 0) add(AttentionItem.FAILED_CHECK)
    if (model.proof?.state == "unavailable") add(AttentionItem.FAILED_CHECK)
    if (model.contract?.malformed != null) add(AttentionItem.AMBIGUOUS_REQUIREMENT)
    return if (out.size <= max) out else out.subList(0, max)
}
