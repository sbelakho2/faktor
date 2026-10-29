// The in-memory durable transcript model: upsert by stable message ID with a
// content-revision guard. The durable `seq` determines ORDERING (replay
// position) only — it never decides whether an existing row's content may
// update. The pre-fix renderer kept a single `renderedSeq` high-water mark
// and filtered every later page to `seq > renderedSeq`, so a message first
// observed EMPTY or PARTIAL stayed truncated forever: its later content rode
// the same sequence and was filtered permanently (P0 "permanently
// incomplete replies").
//
// The model is a pure id-keyed map with ordered indices:
//  - rows are keyed by stable identity: durable messages by their durable
//    message id, the optimistic user line by its submission id;
//  - durable rows are kept sorted by (seq, id) — any arrival order converges
//    to the same transcript order;
//  - each row carries a content REVISION. Production derives it from the
//    durable text length ([contentRevision]): durable message text only grows
//    (parts are appended), so a strictly shorter observation is a stale
//    snapshot and is REFUSED by the revision guard (a downward revision
//    never happens in production; the guard proves the rule). Ties accept
//    the newest observation, so an equal-length rewrite still converges;
//  - the optimistic user row is reconciled with its durable counterpart by
//    IDENTITY (same role + same text), never by duplication: the provisional
//    row MOVES to the durable row's ordered position under the durable key.
//
// Turn grouping (one Faktor turn presentation per logical turn):
//  - the live projection carries a `turn` (op) id on every frame/snapshot
//    part; a run of consecutive assistant rows SHARES one group, and a
//    known turn id SUBDIVIDES the run (rows of turn op-1 vs op-2 are two
//    turns even without an intervening user row);
//  - the durable projection (NativeMessage) carries NO turn field, so the
//    documented FALLBACK is role-run grouping: a maximal run of consecutive
//    assistant rows (ordered by seq, live overlay merged) is ONE turn; any
//    intervening user row starts a new turn. Group ids derive from the
//    first row of the run, so an unchanged topology never churns payloads.
//
// Rendering is the sink's job (TranscriptSink row ops); this model only
// computes the ordered rows and the minimal diff ops. EDT-only: the panel
// mutates it from EDT apply blocks.
package dev.faktor.frontend

import dev.faktor.backend.LiveChunkFrame
import dev.faktor.backend.LiveSnapshot
import dev.faktor.shared.NativeMessage

/** UTF-8 byte length of a Kotlin string (surrogate pairs count once). */
internal fun liveUtf8Length(text: String): Long {
    var bytes = 0L
    var index = 0
    while (index < text.length) {
        val c = text[index]
        bytes += when {
            c.code < 0x80 -> 1
            c.code < 0x800 -> 2
            c.isHighSurrogate() && index + 1 < text.length && text[index + 1].isLowSurrogate() -> {
                index += 1
                4
            }
            else -> 3
        }
        index += 1
    }
    return bytes
}

/**
 * One live part lineage. [bytes] is the running UTF-8 length of [text] —
 * offsets are NEVER UTF-16 indices; [attempt] pins the snapshot lineage and
 * [revision] is the per-part publish counter. [turn] is the live op id the
 * part belongs to (grouping authority while the stream is hot).
 */
internal data class LivePartState(
    val messageId: Long,
    val kind: String,
    val text: String,
    val bytes: Long,
    val attempt: Int?,
    val revision: Long,
    val sequence: Long,
    val turn: String? = null
)

/** One ordered discrete (tool) part row rendered between text rows. */
internal data class LiveToolSegment(
    val messageId: Long,
    val kind: String,
    val sequence: Long,
    val content: String,
    val turn: String? = null
)

/** The stable identity of one transcript row. */
internal data class TranscriptRowKey(val kind: String, val id: String) {
    companion object {
        const val KIND_DURABLE = "m"
        const val KIND_PROVISIONAL = "p"

        fun durable(id: Long): TranscriptRowKey =
            TranscriptRowKey(KIND_DURABLE, id.toString())

        fun provisional(submissionId: String): TranscriptRowKey =
            TranscriptRowKey(KIND_PROVISIONAL, submissionId)
    }

    /** The sink-side render key (stable across a session). */
    fun render(): String = "$kind:$id"
}

/** One transcript row: identity, ordering position and revisioned content. */
internal data class TranscriptRow(
    val key: TranscriptRowKey,
    /** The durable ordering position. Provisional rows sort after every
     * durable row and never take part in seq ordering. */
    val seq: Long,
    /** The durable role ("user"/"assistant"/...); provisional rows render
     * as the optimistic "You" line. */
    val role: String,
    val text: String,
    val revision: Long,
    /** The live turn (op) id when the row was observed through the stream. */
    val turn: String? = null
) {
    companion object {
        const val PROVISIONAL_ROLE = "you"

        /** The effective speaker role (the provisional row is a user line). */
        fun speakerOf(role: String): String =
            if (role == PROVISIONAL_ROLE) "user" else role
    }
}

/**
 * The content revision of one observation. Durable message text is
 * append-only (the daemon joins a message's parts in order), so the text
 * length is a monotonic production revision: growth advances it, a stale
 * snapshot regresses it, and the model's guard refuses regressions.
 */
internal fun contentRevision(text: String): Long = text.length.toLong()

/**
 * One render instruction for the sink:
 *  - [Upsert]: render [payload] for [key] — replace in place when the row
 *    is currently rendered, insert before [beforeKey] (null = document end)
 *    when it is not (a new row, or a row whose text just became non-empty);
 *  - [Rekey]: the provisional row [key] becomes the durable row [newKey]
 *    with [payload], repositioned to the durable ordered position (the sink
 *    removes the old range and inserts at the resolved anchor).
 */
internal sealed class TranscriptRowOp {
    abstract val key: TranscriptRowKey

    data class Upsert(
        override val key: TranscriptRowKey,
        val payload: TranscriptRowPayload,
        val beforeKey: TranscriptRowKey?
    ) : TranscriptRowOp()

    data class Rekey(
        override val key: TranscriptRowKey,
        val newKey: TranscriptRowKey,
        val payload: TranscriptRowPayload,
        val beforeKey: TranscriptRowKey?
    ) : TranscriptRowOp()
}

/**
 * The ordered, revisioned transcript rows of ONE session. Pure logic; the
 * panel applies the returned ops to the document sink. All mutations happen
 * on the EDT (single-threaded by construction).
 */
internal class TranscriptModel {

    /** Id-keyed rows (the stable map); the ordered index is [order]. */
    private val rows = LinkedHashMap<TranscriptRowKey, TranscriptRow>()

    /** Ordered row keys: durable rows by (seq, id), provisional last. */
    private val order = ArrayList<TranscriptRowKey>()

    /** The last payload emitted per key (the minimal-diff baseline). */
    private val emitted = HashMap<TranscriptRowKey, TranscriptRowPayload>()

    /** Stale-snapshot refusals (the revision guard), for smoke observability. */
    var revisionGuardHits: Long = 0
        private set

    // ------------------------------------------------------ live overlay
    //
    // Live frames are applied to a PART-KEYED overlay (`messageId:kind`)
    // before/alongside the durable rows. Byte offsets are UTF-8; a live
    // frame's whole-code-point `content` is appended and the running byte
    // counter advances by its UTF-8 length — no UTF-16 index ever enters the
    // offset arithmetic. The overlay is reconciled against every durable
    // observation: durable wins divergence, a saved page never overwrites a
    // newer live suffix, and an in-order frame continues exactly at the
    // snapshot offset.

    private val liveParts = LinkedHashMap<String, LivePartState>()

    private val liveTools = LinkedHashMap<String, LiveToolSegment>()

    /** The server-instance id of the last handshake (foreign frames drop). */
    var liveExpectedInstance: String? = null
        private set

    var liveForeignDropped: Long = 0
        private set

    var liveDuplicatesDropped: Long = 0
        private set

    var liveGapSignals: Long = 0
        private set

    /** The turn group of the newest observed assistant part (approval parts). */
    private var lastAssistantGroup: String? = null

    /** The explicit resync signal: the caller fetches the authoritative snapshot. */
    var liveGapListener: ((String) -> Unit)? = null
        private set

    fun setLiveGapListener(listener: ((String) -> Unit)?) {
        liveGapListener = listener
    }

    fun reset() {
        rows.clear()
        order.clear()
        emitted.clear()
        revisionGuardHits = 0
        lastAssistantGroup = null
        resetLive()
    }

    /** Clears only the live overlay (durable rows survive a reconnect). */
    fun resetLive() {
        liveParts.clear()
        liveTools.clear()
        liveExpectedInstance = null
        liveForeignDropped = 0
        liveDuplicatesDropped = 0
        liveGapSignals = 0
    }

    private fun liveKey(messageId: Long, kind: String): String = "$messageId:$kind"

    /** Bounded part lineages: the oldest part is evicted past the cap. */
    private fun boundLiveParts() {
        while (liveParts.size > MAX_LIVE_PARTS) {
            val oldest = liveParts.keys.firstOrNull() ?: break
            liveParts.remove(oldest)
        }
    }

    /** The durable floor of a part: the saved message text (reasoning has none). */
    private fun durableFloor(messageId: Long, kind: String): String {
        if (kind != "text") return ""
        return rows[TranscriptRowKey.durable(messageId)]?.text ?: ""
    }

    /**
     * Apply ONE live chunk to the overlay. Returns true only when visible
     * text advanced. Duplicate/replayed revisions are dropped BEFORE any DOM
     * work; an offset/revision gap requests an explicit resync and never
     * fabricates text.
     */
    fun applyLiveChunk(frame: LiveChunkFrame): Boolean {
        val expected = liveExpectedInstance
        if (expected == null) {
            liveExpectedInstance = frame.instance
        } else if (frame.instance != expected) {
            // A changed server instance is a foreign lineage (daemon restart):
            // the new handshake snapshot is authority.
            liveForeignDropped += 1
            return false
        }
        if (frame.content.isEmpty()) return false
        val kind = frame.kind
        if (kind != "text" && kind != "reasoning") {
            // Discrete kinds (tool payloads) render as their own ordered row.
            val toolKey = liveKey(frame.messageId, "$kind@${frame.sequence}")
            liveTools[toolKey] = LiveToolSegment(
                frame.messageId, kind, frame.sequence, frame.content, frame.turn
            )
            // Bounded: a hostile tool-frame flood evicts the oldest rows.
            while (liveTools.size > MAX_LIVE_TOOL_ROWS) {
                val oldest = liveTools.keys.firstOrNull() ?: break
                liveTools.remove(oldest)
            }
            return true
        }
        val key = liveKey(frame.messageId, kind)
        var part = liveParts[key]
        if (part == null) {
            // Byte offsets are absolute: seed from the durable floor so a
            // live part can start above an already-rendered prefix.
            val base = durableFloor(frame.messageId, kind)
            part = LivePartState(
                messageId = frame.messageId,
                kind = kind,
                text = base,
                bytes = liveUtf8Length(base),
                attempt = null,
                revision = -1,
                sequence = frame.sequence,
                turn = frame.turn
            )
            liveParts[key] = part
            boundLiveParts()
        }
        if (part.attempt != null && part.attempt != frame.attempt) {
            // Lineage epoch changed (a snapshot rebase): one resync, never a
            // merge of two lineages.
            liveParts.remove(key)
            requestGap(key)
            return false
        }
        var accepted = part
        if (part.attempt == null) accepted = accepted.copy(attempt = frame.attempt)
        if (accepted.turn == null && frame.turn != null) {
            accepted = accepted.copy(turn = frame.turn)
        }
        if (frame.revision <= accepted.revision) {
            // Duplicate/replayed frame: dropped before any visible work.
            liveDuplicatesDropped += 1
            return false
        }
        if (accepted.revision >= 0 && frame.revision > accepted.revision + 1) {
            requestGap(key)
            return false
        }
        val contentBytes = liveUtf8Length(frame.content)
        if (frame.offset > accepted.bytes) {
            requestGap(key)
            return false
        }
        if (frame.offset < accepted.bytes) {
            if (frame.offset + contentBytes <= accepted.bytes) {
                // Fully held already (the duplicate covers only known bytes).
                liveParts[key] = accepted.copy(revision = frame.revision)
                liveDuplicatesDropped += 1
                return false
            }
            requestGap(key)
            return false
        }
        // Whole code points: append the content string and advance the BYTE
        // counter by that content's UTF-8 length (never a UTF-16 index).
        liveParts[key] = accepted.copy(
            text = accepted.text + frame.content,
            bytes = accepted.bytes + contentBytes,
            revision = frame.revision,
            sequence = frame.sequence
        )
        return true
    }

    /**
     * Reconcile the authoritative snapshot into the overlay:
     *   * snapshot extends held → adopt it (we were behind);
     *   * held extends the snapshot → KEEP the newer live suffix (a saved
     *     snapshot never overwrites it);
     *   * divergence → the durable/composed authority wins (byte-identical
     *     convergence; the lineage re-pins on the next frame/snapshot).
     */
    fun applyLiveSnapshot(snapshot: LiveSnapshot) {
        val expected = liveExpectedInstance
        if (expected != null && expected != snapshot.instance) {
            // A daemon restart: the old lineage cannot be mixed with the new.
            liveParts.clear()
            liveTools.clear()
        }
        liveExpectedInstance = snapshot.instance
        for (part in snapshot.parts) {
            val kind = part.kind
            if (kind != "text" && kind != "reasoning") continue
            val key = liveKey(part.messageId, kind)
            val snapText = part.text
            val snapBytes = part.offset
            val held = liveParts[key]
            if (held == null) {
                if (snapBytes > 0) {
                    liveParts[key] = LivePartState(
                        part.messageId, kind, snapText, snapBytes,
                        part.attempt, part.revision, snapshot.sequence, part.turn
                    )
                    boundLiveParts()
                }
                continue
            }
            val updated = when {
                snapText.startsWith(held.text) -> {
                    // Snapshot at least as new: adopt byte-exactly.
                    held.copy(
                        text = snapText,
                        bytes = snapBytes,
                        attempt = part.attempt,
                        sequence = snapshot.sequence,
                        turn = part.turn ?: held.turn
                    )
                }
                held.text.startsWith(snapText) -> {
                    // The snapshot is a prefix: keep the newer live suffix.
                    held
                }
                else -> {
                    // Divergence: the authority wins; the next frame re-pins
                    // the lineage (attempt = null until then).
                    held.copy(
                        text = snapText,
                        bytes = snapBytes,
                        attempt = null,
                        sequence = snapshot.sequence,
                        turn = part.turn ?: held.turn
                    )
                }
            }
            liveParts[key] = updated.copy(revision = maxOf(updated.revision, part.revision))
        }
    }

    private fun requestGap(key: String) {
        liveGapSignals += 1
        liveGapListener?.invoke(key)
    }

    /** Durable text always wins a divergence; a live suffix is preserved. */
    private fun reconcileLiveWithDurable() {
        for ((key, part) in liveParts.entries.toList()) {
            if (part.kind != "text") continue
            val base = durableFloor(part.messageId, part.kind)
            if (!part.text.startsWith(base)) {
                liveParts[key] = part.copy(
                    text = base,
                    bytes = liveUtf8Length(base),
                    attempt = null
                )
            }
        }
    }

    /**
     * The ordered rows AS RENDERED: durable rows merged with the live text,
     * plus a synthetic assistant row for a message whose first live token
     * arrives before any durable page contains it (first-token streaming).
     */
    fun rowsWithLive(): List<TranscriptRow> {
        reconcileLiveWithDurable()
        val base = rows()
        val out = ArrayList<TranscriptRow>(base.size + 4)
        out.addAll(base)
        for (part in liveParts.values) {
            if (part.kind != "text" || part.text.isEmpty()) continue
            val key = TranscriptRowKey.durable(part.messageId)
            val index = out.indexOfFirst { it.key == key }
            if (index >= 0) {
                val row = out[index]
                if (row.text != part.text) {
                    out[index] = row.copy(
                        text = part.text,
                        revision = maxOf(row.revision, contentRevision(part.text)),
                        turn = part.turn ?: row.turn
                    )
                } else if (part.turn != null && row.turn == null) {
                    out[index] = row.copy(turn = part.turn)
                }
            } else {
                out.add(
                    TranscriptRow(
                        key,
                        LIVE_ROW_SEQ_BASE + part.messageId,
                        "assistant",
                        part.text,
                        contentRevision(part.text),
                        part.turn
                    )
                )
            }
        }
        return out
    }

    /**
     * Render ops for the live overlay: one in-place update per row whose
     * merged payload moved (text OR turn group), plus the ordered discrete
     * (tool) rows. The sink no-ops an unchanged payload, so a quiet flush
     * performs no DOM work.
     */
    fun liveOps(): List<TranscriptRowOp> = emitChanged({ emitted.containsKey(it) })

    /**
     * Fold one durable message page into the model. Rows are processed in
     * seq order; for each message:
     *  - an EXISTING id updates its content when the incoming revision is
     *    not lower (seq never gates the update — that was the P0 defect);
     *  - a user message first reconciles any matching optimistic row by
     *    identity (same text), rekeying it to the durable id instead of a
     *    duplicate row;
     *  - a NEW id inserts at its seq-ordered position (out-of-order arrivals
     *    converge to the same order).
     * [located] reports whether a row key is currently rendered in the
     * document, so insertion anchors skip not-yet-rendered rows.
     */
    fun upsertDurable(
        messages: List<NativeMessage>,
        located: (TranscriptRowKey) -> Boolean
    ): List<TranscriptRowOp> {
        val rekeys = ArrayList<Pair<TranscriptRowKey, TranscriptRowKey>>()
        val sorted = messages.sortedWith(compareBy({ it.seq }, { it.id }))
        for (message in sorted) {
            val key = TranscriptRowKey.durable(message.id)
            val revision = contentRevision(message.text)
            val existing = rows[key]
            if (existing != null) {
                if (revision < existing.revision) {
                    revisionGuardHits += 1
                    continue
                }
                if (existing.text == message.text && existing.role == message.role) continue
                rows[key] = existing.copy(
                    seq = message.seq,
                    role = message.role,
                    text = message.text,
                    revision = revision
                )
                continue
            }
            // Reconcile the optimistic user line by identity BEFORE inserting
            // a durable twin: one row, never two.
            val provisionalKey = if (message.role == "user") {
                order.firstOrNull { candidate ->
                    val row = rows[candidate] ?: return@firstOrNull false
                    row.key.kind == TranscriptRowKey.KIND_PROVISIONAL &&
                        row.text.trim() == message.text.trim()
                }
            } else {
                null
            }
            val row = TranscriptRow(key, message.seq, message.role, message.text, revision)
            val insertAt = orderedInsertIndex(row)
            if (provisionalKey != null) {
                // The provisional slot is at or after insertAt (provisional
                // rows sort last), so removing it first keeps insertAt valid.
                rows.remove(provisionalKey)
                order.remove(provisionalKey)
                emitted.remove(provisionalKey)
                rows[key] = row
                order.add(insertAt, key)
                rekeys.add(provisionalKey to key)
                continue
            }
            rows[key] = row
            order.add(insertAt, key)
        }
        return emitChanged(located, rekeys)
    }

    /**
     * The minimal op set for the CURRENT visible state: every row whose
     * payload (text, role, or turn group) differs from the last emitted one,
     * plus the recorded provisional rekeys. Group ids are computed over the
     * merged durable+live order in ONE pass, so a run split by an arriving
     * user row or a turn change re-emits exactly the affected rows.
     */
    private fun emitChanged(
        located: (TranscriptRowKey) -> Boolean,
        rekeys: List<Pair<TranscriptRowKey, TranscriptRowKey>> = emptyList()
    ): List<TranscriptRowOp> {
        val visible = rowsWithLive()
        val groups = computeGroups(visible)
        lastAssistantGroup = visible.lastOrNull {
            TranscriptRow.speakerOf(it.role) == "assistant"
        }?.let { groups[it.key] }
        val ops = ArrayList<TranscriptRowOp>()
        val rekeyed = HashSet<TranscriptRowKey>()
        for ((oldKey, newKey) in rekeys) {
            emitted.remove(oldKey)
            val index = visible.indexOfFirst { it.key == newKey }
            if (index < 0) continue
            val row = visible[index]
            if (row.text.isEmpty()) continue
            val payload = payloadOf(row, groups[newKey] ?: fallbackGroup(newKey))
            emitted[newKey] = payload
            rekeyed.add(newKey)
            ops.add(
                TranscriptRowOp.Rekey(
                    oldKey, newKey, payload,
                    nextLocatedKey(visible, index, located)
                )
            )
        }
        for ((index, row) in visible.withIndex()) {
            if (row.key in rekeyed || row.text.isEmpty()) continue
            val payload = payloadOf(row, groups[row.key] ?: fallbackGroup(row.key))
            if (emitted[row.key] == payload) continue
            emitted[row.key] = payload
            ops.add(
                TranscriptRowOp.Upsert(row.key, payload, nextLocatedKey(visible, index, located))
            )
        }
        for (tool in liveTools.values.sortedBy { it.sequence }) {
            val key = TranscriptRowKey(
                LIVE_TOOL_KIND, "${tool.messageId}:${tool.kind}@${tool.sequence}"
            )
            val payload = TranscriptRowPayload(
                kind = TranscriptRowPayload.KIND_TOOL,
                text = tool.content,
                seq = tool.sequence,
                rowId = tool.messageId.toString(),
                group = groups[TranscriptRowKey.durable(tool.messageId)] ?: "",
                label = tool.kind
            )
            if (emitted[key] == payload) continue
            emitted[key] = payload
            ops.add(TranscriptRowOp.Upsert(key, payload, null))
        }
        return ops
    }

    /** The render payload of one row inside its turn group. */
    private fun payloadOf(row: TranscriptRow, group: String): TranscriptRowPayload =
        when (TranscriptRow.speakerOf(row.role)) {
            "assistant" -> TranscriptRowPayload(
                TranscriptRowPayload.KIND_ASSISTANT, row.text, row.seq, row.key.id, group
            )
            "user" -> TranscriptRowPayload(
                TranscriptRowPayload.KIND_USER, row.text,
                if (row.key.kind == TranscriptRowKey.KIND_PROVISIONAL) -1 else row.seq,
                row.key.id, group
            )
            else -> TranscriptRowPayload(
                row.role, row.text, row.seq, row.key.id, group
            )
        }

    private fun fallbackGroup(key: TranscriptRowKey): String = "g:${key.kind}:${key.id}"

    /**
     * Turn groups over the merged visible order (documented fallback when
     * the projection carries no turn id — see the file header):
     *  - non-assistant rows always start their own group;
     *  - consecutive assistant rows continue the current run unless BOTH
     *    the current and the previous row carry KNOWN, DIFFERENT turn ids;
     *  - a group id derives from the FIRST row of its run, so an unchanged
     *    topology produces byte-identical payloads (no render churn).
     */
    private fun computeGroups(visible: List<TranscriptRow>): Map<TranscriptRowKey, String> {
        val groups = HashMap<TranscriptRowKey, String>(visible.size)
        var currentGroup: String? = null
        var currentAssistant = false
        var currentTurn: String? = null
        for (row in visible) {
            val isAssistant = TranscriptRow.speakerOf(row.role) == "assistant"
            val continues = isAssistant && currentAssistant &&
                (row.turn == null || currentTurn == null || row.turn == currentTurn)
            if (!continues) {
                currentGroup = fallbackGroup(row.key)
                currentTurn = null
            }
            if (isAssistant && row.turn != null) currentTurn = row.turn
            currentAssistant = isAssistant
            groups[row.key] = currentGroup!!
        }
        return groups
    }

    /**
     * The nearest RENDERED row key after [fromIndex] (the insertion anchor);
     * null means "append at the document end". Rows with empty text (reserved
     * but not rendered) and rows lost to the transcript trim are skipped, so
     * an insert always lands at a real document position.
     */
    private fun nextLocatedKey(
        visible: List<TranscriptRow>,
        fromIndex: Int,
        located: (TranscriptRowKey) -> Boolean
    ): TranscriptRowKey? {
        for (index in (fromIndex + 1) until visible.size) {
            if (visible[index].text.isEmpty()) continue
            if (located(visible[index].key)) return visible[index].key
        }
        return null
    }

    /** The turn group of the newest observed assistant part (null = none). */
    fun currentAssistantGroup(): String? = lastAssistantGroup

    /** Bounded live-overlay diagnostics for the smoke/tests. */
    fun livePartsForTest(): Map<String, LivePartState> = LinkedHashMap(liveParts)

    /** The ordered rows (durable first by seq, provisional last). */
    fun rows(): List<TranscriptRow> = order.mapNotNull { rows[it] }

    /** The row key of one durable seq (message navigation), or null. */
    fun keyForSeq(seq: Long): TranscriptRowKey? =
        order.firstOrNull { rows[it]?.seq == seq }

    /**
     * The optimistic user line: one provisional row per logical submission.
     * A repeat of the same submission id updates the row (the transport
     * retry of the same draft) instead of duplicating it. The row renders at
     * the document end; durable rows insert before it until it reconciles.
     */
    fun addProvisional(submissionId: String, text: String): TranscriptRowOp? {
        val key = TranscriptRowKey.provisional(submissionId)
        val revision = contentRevision(text)
        val existing = rows[key]
        if (existing != null) {
            if (revision < existing.revision) {
                revisionGuardHits += 1
                return null
            }
            if (existing.text == text) return null
            rows[key] = existing.copy(text = text, revision = revision)
        } else {
            rows[key] = TranscriptRow(
                key, Long.MAX_VALUE, TranscriptRow.PROVISIONAL_ROLE, text, revision
            )
            order.add(key)
        }
        val payload = payloadOf(rows[key]!!, fallbackGroup(key))
        emitted[key] = payload
        return TranscriptRowOp.Upsert(key, payload, null)
    }

    /** The sorted insertion index of a new durable row. */
    private fun orderedInsertIndex(row: TranscriptRow): Int {
        for (index in order.indices) {
            val other = rows[order[index]] ?: continue
            if (other.key.kind == TranscriptRowKey.KIND_PROVISIONAL) return index
            if (other.seq > row.seq || (other.seq == row.seq && other.key.id > row.key.id)) {
                return index
            }
        }
        return order.size
    }

    companion object {
        /** Synthetic live rows sort after durable rows (by message id). */
        private const val LIVE_ROW_SEQ_BASE = Long.MAX_VALUE / 2

        const val LIVE_TOOL_KIND = "lt"

        /** Hard bounds of the part-keyed live overlay (bounded everything). */
        const val MAX_LIVE_PARTS = 512
        const val MAX_LIVE_TOOL_ROWS = 256
    }
}
