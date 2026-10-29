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
// Rendering is the sink's job (TranscriptSink row ops); this model only
// computes the ordered rows and the minimal diff ops. EDT-only: the panel
// mutates it from EDT apply blocks.
package dev.faktor.frontend

import dev.faktor.shared.NativeMessage

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
     * as the optimistic "you:" line. */
    val role: String,
    val text: String,
    val revision: Long
) {
    /** The exact rendered line (without the trailing newline). */
    val line: String
        get() = if (key.kind == TranscriptRowKey.KIND_PROVISIONAL) {
            "you: $text"
        } else {
            "$role: $text"
        }

    companion object {
        const val PROVISIONAL_ROLE = "you"
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
 *  - [Upsert]: render [line] for [key] — replace in place when the row is
 *    currently rendered, insert before [beforeKey] (null = document end)
 *    when it is not (a new row, or a row whose text just became non-empty);
 *  - [Rekey]: the provisional row [key] becomes the durable row [newKey]
 *    with [line], repositioned to the durable ordered position (the sink
 *    removes the old range and inserts at the resolved anchor).
 */
internal sealed class TranscriptRowOp {
    abstract val key: TranscriptRowKey

    data class Upsert(
        override val key: TranscriptRowKey,
        val line: String,
        val beforeKey: TranscriptRowKey?
    ) : TranscriptRowOp()

    data class Rekey(
        override val key: TranscriptRowKey,
        val newKey: TranscriptRowKey,
        val line: String,
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

    /** Stale-snapshot refusals (the revision guard), for smoke observability. */
    var revisionGuardHits: Long = 0
        private set

    fun reset() {
        rows.clear()
        order.clear()
        revisionGuardHits = 0
    }

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
            return TranscriptRowOp.Upsert(key, "you: $text", null)
        }
        rows[key] = TranscriptRow(key, Long.MAX_VALUE, TranscriptRow.PROVISIONAL_ROLE, text, revision)
        order.add(key)
        return TranscriptRowOp.Upsert(key, "you: $text", null)
    }

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
        val ops = ArrayList<TranscriptRowOp>()
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
                val updated = existing.copy(
                    seq = message.seq,
                    role = message.role,
                    text = message.text,
                    revision = revision
                )
                rows[key] = updated
                if (updated.text.isEmpty()) continue
                ops.add(
                    TranscriptRowOp.Upsert(
                        key, updated.line,
                        nextLocatedKey(order.indexOf(key), located)
                    )
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
                rows[key] = row
                order.add(insertAt, key)
                if (row.text.isNotEmpty()) {
                    ops.add(
                        TranscriptRowOp.Rekey(
                            provisionalKey, key, row.line,
                            nextLocatedKey(order.indexOf(key), located)
                        )
                    )
                }
                continue
            }
            rows[key] = row
            order.add(insertAt, key)
            if (row.text.isNotEmpty()) {
                ops.add(
                    TranscriptRowOp.Upsert(key, row.line, nextLocatedKey(insertAt, located))
                )
            }
        }
        return ops
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

    /**
     * The nearest row key after [fromIndex] that is currently RENDERED in
     * the document (the insertion anchor); null means "append at the
     * document end". Rows with empty text (reserved but not rendered) and
     * rows lost to the transcript trim are skipped, so an insert always
     * lands at a real document position.
     */
    private fun nextLocatedKey(
        fromIndex: Int,
        located: (TranscriptRowKey) -> Boolean
    ): TranscriptRowKey? {
        for (index in (fromIndex + 1) until order.size) {
            if (located(order[index])) return order[index]
        }
        return null
    }
}
