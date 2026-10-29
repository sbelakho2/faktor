// The ONE turn-lifecycle reducer for the JetBrains panel (port of
// `apps/vscode/src/turnState.ts`; identical state machine and rules).
//
// States (exactly one per tracked submission):
//
//   idle → submitting → queued | starting → streaming | tool-running |
//   awaiting-user → completed | interrupted | failed
//
// Rules the reducer enforces:
//   * a receipt (op/run ids) moves submitting → queued or starting — never
//     straight to a terminal state (HTTP 2xx is admission, not completion);
//   * terminal transitions come ONLY from durable authority: the turn record
//     status (`turnStatus`), the durable journal's `turn_completed`/`failed`
//     events, or the typed abort ack (`stopped`) — never from a quiet stream;
//   * a machine-state tag maps to a NON-terminal phase only (a quiet/ready
//     machine never completes a turn by itself);
//   * terminal entries are sticky until a NEW submit supersedes them, and the
//     entry list is bounded (stale terminal entries are pruned);
//   * Stop targets the FIRST non-terminal entry that has an operation id and
//     is actually running (a queued follow-up is never the Stop target — the
//     streaming turn ahead of it is).
//
// Pure module: no Swing, no clock, no I/O; the tests drive every transition
// directly.
package dev.faktor.frontend

internal enum class TurnPhase(val wire: String) {
    IDLE("idle"),
    SUBMITTING("submitting"),
    QUEUED("queued"),
    STARTING("starting"),
    STREAMING("streaming"),
    TOOL_RUNNING("tool-running"),
    AWAITING_USER("awaiting-user"),
    COMPLETED("completed"),
    INTERRUPTED("interrupted"),
    FAILED("failed");

    companion object {
        fun fromWire(value: String): TurnPhase? = values().firstOrNull { it.wire == value }
    }
}

internal enum class TurnSource(val wire: String) {
    ASK("ask"),
    TASK("task")
}

internal data class TurnEntry(
    /** The admission receipt's operation id (null until the receipt lands). */
    val opId: String?,
    val runId: String?,
    val source: TurnSource,
    val phase: TurnPhase
)

internal data class TurnLifecycle(val entries: List<TurnEntry>) {
    companion object {
        /** Hard bound of tracked submissions (bounded everything). */
        const val MAX_TURNS = 4

        fun empty(): TurnLifecycle = TurnLifecycle(emptyList())
    }
}

internal sealed class TurnEvent {
    data class Submit(val source: TurnSource) : TurnEvent()
    data class Receipt(
        val opId: String?,
        val runId: String? = null,
        val queued: Boolean
    ) : TurnEvent()
    data class Admitted(val opId: String) : TurnEvent()
    data class Machine(val state: String, val opId: String? = null) : TurnEvent()
    data class Journal(
        val kind: String,
        val opId: String? = null,
        val state: String? = null
    ) : TurnEvent()
    data class TurnStatus(val opId: String?, val status: String) : TurnEvent()
    data class Stopped(val opId: String) : TurnEvent()
    object Reset : TurnEvent()
}

internal fun isTerminalPhase(phase: TurnPhase): Boolean =
    phase == TurnPhase.COMPLETED || phase == TurnPhase.INTERRUPTED || phase == TurnPhase.FAILED

private fun isLivePhase(phase: TurnPhase): Boolean =
    phase == TurnPhase.SUBMITTING ||
        phase == TurnPhase.QUEUED ||
        phase == TurnPhase.STARTING ||
        phase == TurnPhase.STREAMING ||
        phase == TurnPhase.TOOL_RUNNING ||
        phase == TurnPhase.AWAITING_USER

private fun isStoppablePhase(phase: TurnPhase): Boolean =
    phase == TurnPhase.STARTING ||
        phase == TurnPhase.STREAMING ||
        phase == TurnPhase.TOOL_RUNNING ||
        phase == TurnPhase.AWAITING_USER

/** True while ANY tracked submission is unfinished (drives `busy`). */
internal fun turnLifecycleIsLive(state: TurnLifecycle): Boolean =
    state.entries.any { isLivePhase(it.phase) }

/** The operation Stop must abort (the first running entry with an op id). */
internal fun turnStopOpId(state: TurnLifecycle): String? {
    for (entry in state.entries) {
        if (isStoppablePhase(entry.phase) && entry.opId != null) return entry.opId
    }
    return null
}

/**
 * The QUEUED follow-ups waiting behind the active turn, in submission
 * order. Each is individually removable through the EXISTING typed abort
 * route (its `op_id` cancels that queued prompt's durable row without
 * touching the state machine or the operation Stop targets) — a queued
 * follow-up is never the Stop target.
 */
internal fun turnQueuedOpIds(state: TurnLifecycle): List<String> {
    val queued = ArrayList<String>()
    for (entry in state.entries) {
        if (entry.phase == TurnPhase.QUEUED && entry.opId != null) queued.add(entry.opId)
    }
    return queued
}

/**
 * The one display phase: the NEWEST submission's phase (a queued follow-up
 * therefore renders `Queued` while an earlier turn still streams), else the
 * newest live/terminal outcome, else idle.
 */
internal fun turnDisplayPhase(state: TurnLifecycle): TurnPhase {
    if (state.entries.isEmpty()) return TurnPhase.IDLE
    val newest = state.entries[state.entries.size - 1]
    if (newest.phase != TurnPhase.IDLE) return newest.phase
    for (index in state.entries.indices.reversed()) {
        val phase = state.entries[index].phase
        if (phase != TurnPhase.IDLE) return phase
    }
    return TurnPhase.IDLE
}

/** The product copy for one phase (null for idle: fall back to Ready). */
internal fun turnPhaseLabel(phase: TurnPhase): String? = when (phase) {
    TurnPhase.SUBMITTING -> "Sending…"
    TurnPhase.QUEUED -> "Queued"
    TurnPhase.STARTING -> "Starting…"
    TurnPhase.STREAMING -> "Generating…"
    TurnPhase.TOOL_RUNNING -> "Working…"
    TurnPhase.AWAITING_USER -> "Needs approval"
    TurnPhase.COMPLETED -> "Completed"
    TurnPhase.INTERRUPTED -> "Stopped"
    TurnPhase.FAILED -> "Failed"
    TurnPhase.IDLE -> null
}

/** AgentState tag → non-terminal phase (never a terminal transition). */
private fun phaseFromMachineState(state: String): TurnPhase? = when (state) {
    "preparing", "building_context" -> TurnPhase.STARTING
    "waiting_for_model", "streaming" -> TurnPhase.STREAMING
    "tool_requested", "executing_tool", "updating_memory", "validating" ->
        TurnPhase.TOOL_RUNNING
    "waiting_for_permission", "needs_user_input", "suspended" -> TurnPhase.AWAITING_USER
    else -> null
}

private fun prune(entries: List<TurnEntry>): List<TurnEntry> {
    if (entries.size <= TurnLifecycle.MAX_TURNS) return entries
    val out = ArrayList<TurnEntry>()
    var dropped = 0
    val drop = entries.size - TurnLifecycle.MAX_TURNS
    for (entry in entries) {
        if (dropped < drop && isTerminalPhase(entry.phase)) {
            dropped += 1
            continue
        }
        out.add(entry)
    }
    while (out.size > TurnLifecycle.MAX_TURNS) out.removeAt(0)
    return out
}

private fun setPhase(entry: TurnEntry, phase: TurnPhase): TurnEntry {
    // Terminal outcomes are sticky for THAT entry: a late machine tag or a
    // stale frame can never resurrect a finished turn.
    if (isTerminalPhase(entry.phase)) return entry
    return entry.copy(phase = phase)
}

private fun entryForOp(entries: List<TurnEntry>, opId: String?): TurnEntry? {
    if (opId != null) return entries.firstOrNull { it.opId == opId }
    // No op id: the newest LIVE (non-terminal) entry.
    for (index in entries.indices.reversed()) {
        val phase = entries[index].phase
        if (!isTerminalPhase(phase) && phase != TurnPhase.IDLE) return entries[index]
    }
    return null
}

private fun replace(entries: MutableList<TurnEntry>, index: Int, updated: TurnEntry) {
    entries[index] = updated
}

/** The one reducer: `turnReducer(state, event) -> state` (pure). */
internal fun turnReducer(state: TurnLifecycle, event: TurnEvent): TurnLifecycle {
    val entries = ArrayList(state.entries)
    when (event) {
        is TurnEvent.Reset -> return TurnLifecycle.empty()
        is TurnEvent.Submit -> {
            entries.add(
                TurnEntry(
                    opId = null,
                    runId = null,
                    source = event.source,
                    phase = TurnPhase.SUBMITTING
                )
            )
            return TurnLifecycle(prune(entries))
        }
        is TurnEvent.Receipt -> {
            // Attach the receipt to the LAST submission still awaiting it (the
            // receipt arrives before any status can possibly change it).
            for (index in entries.indices.reversed()) {
                val entry = entries[index]
                if (entry.opId == null && entry.phase == TurnPhase.SUBMITTING) {
                    replace(
                        entries, index,
                        entry.copy(
                            opId = event.opId,
                            runId = event.runId,
                            phase = if (event.queued) TurnPhase.QUEUED else TurnPhase.STARTING
                        )
                    )
                    break
                }
            }
            return TurnLifecycle(entries)
        }
        is TurnEvent.Admitted -> {
            val index = entries.indexOfFirst { it.opId == event.opId }
            if (index >= 0) {
                val entry = entries[index]
                if (entry.phase == TurnPhase.QUEUED || entry.phase == TurnPhase.SUBMITTING) {
                    replace(entries, index, entry.copy(phase = TurnPhase.STARTING))
                }
                return TurnLifecycle(entries)
            }
            // Admission for an unknown op (another client's turn): adopt it as
            // the active entry only when the newest submission still awaits
            // its own admission.
            if (entries.isNotEmpty()) {
                val newestIndex = entries.size - 1
                val newest = entries[newestIndex]
                if (newest.opId == null) {
                    replace(entries, newestIndex, newest.copy(opId = event.opId, phase = TurnPhase.STARTING))
                } else if (newest.opId == event.opId) {
                    replace(entries, newestIndex, setPhase(newest, TurnPhase.STARTING))
                }
            }
            return TurnLifecycle(entries)
        }
        is TurnEvent.Machine -> {
            val mapped = phaseFromMachineState(event.state) ?: return TurnLifecycle(entries)
            val target = entryForOp(entries, event.opId) ?: entryForOp(entries, null) ?: return TurnLifecycle(entries)
            val index = entries.indexOf(target)
            replace(entries, index, setPhase(target, mapped))
            return TurnLifecycle(entries)
        }
        is TurnEvent.Journal -> {
            val opId = event.opId
            when (event.kind) {
                "prompt_admitted", "prompt_received" -> {
                    val index = if (opId != null) entries.indexOfFirst { it.opId == opId } else -1
                    if (index >= 0) {
                        val entry = entries[index]
                        if (entry.phase == TurnPhase.QUEUED || entry.phase == TurnPhase.SUBMITTING) {
                            replace(entries, index, entry.copy(phase = TurnPhase.STARTING))
                        }
                        return TurnLifecycle(entries)
                    }
                    if (entries.isNotEmpty() && opId != null) {
                        val newestIndex = entries.size - 1
                        val newest = entries[newestIndex]
                        if (newest.opId == null) {
                            replace(
                                entries, newestIndex,
                                newest.copy(opId = opId, phase = TurnPhase.STARTING)
                            )
                        } else if (newest.opId == opId) {
                            replace(entries, newestIndex, setPhase(newest, TurnPhase.STARTING))
                        }
                    }
                    return TurnLifecycle(entries)
                }
                "model_started" -> {
                    val entry = entryForOp(entries, opId) ?: return TurnLifecycle(entries)
                    replace(entries, entries.indexOf(entry), setPhase(entry, TurnPhase.STREAMING))
                    return TurnLifecycle(entries)
                }
                "phase_changed" -> {
                    val mapped = event.state?.let { phaseFromMachineState(it) }
                        ?: return TurnLifecycle(entries)
                    val entry = entryForOp(entries, opId) ?: return TurnLifecycle(entries)
                    replace(entries, entries.indexOf(entry), setPhase(entry, mapped))
                    return TurnLifecycle(entries)
                }
                "tool_started", "tool_completed", "tool_result_appended", "memory_updated" -> {
                    val entry = entryForOp(entries, opId) ?: return TurnLifecycle(entries)
                    replace(entries, entries.indexOf(entry), setPhase(entry, TurnPhase.TOOL_RUNNING))
                    return TurnLifecycle(entries)
                }
                "turn_completed" -> {
                    val entry = entryForOp(entries, opId) ?: return TurnLifecycle(entries)
                    replace(entries, entries.indexOf(entry), setPhase(entry, TurnPhase.COMPLETED))
                    return TurnLifecycle(entries)
                }
                "failed" -> {
                    val entry = entryForOp(entries, opId) ?: return TurnLifecycle(entries)
                    replace(entries, entries.indexOf(entry), setPhase(entry, TurnPhase.FAILED))
                    return TurnLifecycle(entries)
                }
                else -> return TurnLifecycle(entries)
            }
        }
        is TurnEvent.TurnStatus -> {
            val target = entryForOp(entries, event.opId)
            if (target == null) {
                if (event.status == "active" && event.opId != null && !turnLifecycleIsLive(TurnLifecycle(entries))) {
                    // Durable authority reports an ACTIVE turn the local
                    // reducer does not track (daemon restart / fresh session):
                    // adopt it instead of showing idle while still generating.
                    entries.add(
                        TurnEntry(
                            opId = event.opId,
                            runId = null,
                            source = TurnSource.ASK,
                            phase = TurnPhase.STARTING
                        )
                    )
                    return TurnLifecycle(prune(entries))
                }
                return TurnLifecycle(entries)
            }
            val index = entries.indexOf(target)
            when (event.status) {
                "completed" -> replace(entries, index, setPhase(target, TurnPhase.COMPLETED))
                "cancelled" -> replace(entries, index, setPhase(target, TurnPhase.INTERRUPTED))
                "failed" -> replace(entries, index, setPhase(target, TurnPhase.FAILED))
                "active" -> {
                    // Durable authority says the turn IS running: a queued/
                    // submitted/even wrongly-terminal display becomes live
                    // (never a completion).
                    val phase = when {
                        target.phase == TurnPhase.QUEUED || target.phase == TurnPhase.SUBMITTING ->
                            TurnPhase.STARTING
                        isTerminalPhase(target.phase) -> TurnPhase.STARTING
                        else -> target.phase
                    }
                    replace(entries, index, target.copy(phase = phase))
                }
            }
            return TurnLifecycle(entries)
        }
        is TurnEvent.Stopped -> {
            val entry = entryForOp(entries, event.opId) ?: return TurnLifecycle(entries)
            replace(entries, entries.indexOf(entry), setPhase(entry, TurnPhase.INTERRUPTED))
            return TurnLifecycle(entries)
        }
    }
}
