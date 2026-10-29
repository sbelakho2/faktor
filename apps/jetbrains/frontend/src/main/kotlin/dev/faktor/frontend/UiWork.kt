// Swing UI work control: the ONE place a high-rate source (the durable SSE
// stream, which journals per model chunk and can deliver hundreds of frames a
// second) hands work to the EDT. The pump coalesces deltas into a bounded
// flush interval, preserves arrival order, and drains in bounded batches so a
// continuous producer can neither flood the EDT nor starve it. Everything
// else here is the matching render-side discipline:
//
//  - [TranscriptSink]: one document insert per flush (never per delta), a
//    maintained character/line budget, and stable message offsets;
//  - [ScrollPin]/[ScrollPinController]: the follow-vs-detach state machine
//    that honors user scroll intent;
//  - [FormattedValues]: bounded caches keyed by (amount, currency) so a
//    render pass never re-formats the same BigInteger;
//  - [KeyedListSync]: granular list updates keyed by a stable id (no full
//    model reset for an identical snapshot);
//  - [SessionUiScope]: per-session teardown callbacks (queued deltas, view
//    state) disposed on every session switch and on shutdown;
//  - [a11y]/[installActivateKey]: accessible names/descriptions and the one
//    focus-aware activation stroke used by lists and choosers.
package dev.faktor.frontend

import dev.faktor.shared.MicroMoney
import java.awt.event.ActionEvent
import java.awt.event.AdjustmentEvent
import java.math.BigInteger
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong
import javax.swing.AbstractAction
import javax.swing.DefaultComboBoxModel
import javax.swing.DefaultListModel
import javax.swing.JComponent
import javax.swing.JScrollBar
import javax.swing.JScrollPane
import javax.swing.KeyStroke
import javax.swing.SwingUtilities
import javax.swing.Timer

/** The production coalescing interval: 25 render passes/s at most. */
internal const val UI_FLUSH_INTERVAL_MS = 40

/** One EDT flush drains at most this many deltas, then yields. */
internal const val UI_FLUSH_MAX_BATCH = 256

/** Hard queue bound: a hostile producer can never grow the delta queue. */
internal const val UI_MAX_QUEUED = 16_384

/** ...and the queued payload bytes are bounded independently (8 MiB). */
internal const val UI_MAX_QUEUED_BYTES: Long = 8L * 1024 * 1024

/** Slow-surface refresh cadence (messages/board/tree/agents/usage). */
internal const val UI_REFRESH_INTERVAL_MS = 250

/**
 * Thread-safe counters for the UI work harness. Every high-rate path bumps
 * exactly one counter, so a smoke can prove coalescing/batching instead of
 * inferring it from wall time.
 */
internal class UiWorkMetrics {
    val offered = AtomicLong()
    val edtFlushes = AtomicLong()
    val batchedEvents = AtomicLong()
    val maxBatch = AtomicInteger()
    val droppedStaleEvents = AtomicLong()
    val duplicateEvents = AtomicLong()
    val droppedOverflow = AtomicLong()
    val transcriptInserts = AtomicLong()
    val transcriptChars = AtomicLong()
    val transcriptTrims = AtomicLong()
    val renderPasses = AtomicLong()
    val refreshRuns = AtomicLong()
    val sessionScopesDisposed = AtomicLong()
    val sinkFailures = AtomicLong()

    fun recordFlush(batch: Int) {
        if (batch <= 0) return
        edtFlushes.incrementAndGet()
        batchedEvents.addAndGet(batch.toLong())
        var seen = maxBatch.get()
        while (batch > seen && !maxBatch.compareAndSet(seen, batch)) seen = maxBatch.get()
        renderPasses.incrementAndGet()
    }

    fun snapshot(): UiWorkSnapshot = UiWorkSnapshot(
        offered = offered.get(),
        edtFlushes = edtFlushes.get(),
        batchedEvents = batchedEvents.get(),
        maxBatch = maxBatch.get(),
        droppedStaleEvents = droppedStaleEvents.get(),
        duplicateEvents = duplicateEvents.get(),
        droppedOverflow = droppedOverflow.get(),
        transcriptInserts = transcriptInserts.get(),
        transcriptChars = transcriptChars.get(),
        transcriptTrims = transcriptTrims.get(),
        renderPasses = renderPasses.get(),
        refreshRuns = refreshRuns.get(),
        sessionScopesDisposed = sessionScopesDisposed.get(),
        sinkFailures = sinkFailures.get()
    )
}

internal data class UiWorkSnapshot(
    val offered: Long,
    val edtFlushes: Long,
    val batchedEvents: Long,
    val maxBatch: Int,
    val droppedStaleEvents: Long,
    val duplicateEvents: Long,
    val droppedOverflow: Long,
    val transcriptInserts: Long,
    val transcriptChars: Long,
    val transcriptTrims: Long,
    val renderPasses: Long,
    val refreshRuns: Long,
    val sessionScopesDisposed: Long,
    val sinkFailures: Long
) {
    /** Offered deltas per EDT flush: the coalescing factor. */
    val coalescingFactor: Double
        get() = if (edtFlushes <= 0L) 0.0 else offered.toDouble() / edtFlushes.toDouble()
}

/**
 * FIFO delta queue with a hard per-drain bound. Order is the enqueue order
 * (ConcurrentLinkedQueue linearizability); a drain never removes more than
 * [maxBatch] frames, so the EDT returns to the event queue between batches.
 */
internal class CoalescingQueue<T>(
    private val maxBatch: Int = UI_FLUSH_MAX_BATCH,
    private val maxQueued: Int = UI_MAX_QUEUED,
    private val maxQueuedBytes: Long = Long.MAX_VALUE,
    private val weigher: (T) -> Int = { 0 }
) {
    private val queue = ConcurrentLinkedQueue<T>()
    private val count = AtomicInteger()
    private val weight = java.util.concurrent.atomic.AtomicLong()

    /** Enqueues; false when the bounded queue had to drop its oldest frame. */
    @Synchronized
    fun offer(item: T): Boolean {
        val itemWeight = weigher(item).coerceAtLeast(0).toLong()
        queue.add(item)
        count.incrementAndGet()
        weight.addAndGet(itemWeight)
        var dropped = false
        while (count.get() > maxQueued || weight.get() > maxQueuedBytes) {
            val oldest = queue.poll() ?: run {
                count.set(queue.size)
                weight.set(0L)
                break
            }
            count.decrementAndGet()
            weight.addAndGet(-weigher(oldest).coerceAtLeast(0).toLong())
            dropped = true
        }
        return !dropped
    }

    @Synchronized
    fun drain(): List<T> {
        val out = ArrayList<T>(minOf(count.get(), maxBatch).coerceAtLeast(0))
        while (out.size < maxBatch) {
            val item = queue.poll() ?: break
            out.add(item)
        }
        if (out.isNotEmpty()) {
            count.addAndGet(-out.size)
            var drained = 0L
            for (item in out) drained += weigher(item).coerceAtLeast(0).toLong()
            weight.addAndGet(-drained)
        }
        return out
    }

    @get:Synchronized
    val isEmpty: Boolean get() = queue.isEmpty()

    @get:Synchronized
    val size: Int get() = count.get().coerceAtLeast(0)

    @Synchronized
    fun clear() {
        while (queue.poll() != null) {
            // Drain deliberately: the bounded queue owns no external effect.
        }
        count.set(0)
        weight.set(0L)
    }
}

/**
 * Coalescing EDT pump for a high-rate producer. [offer] is thread-safe and
 * cheap (enqueue + arm one timer); the sink always runs on the EDT with a
 * non-empty batch. After the interval elapses the batch flushes; if more
 * deltas arrived meanwhile they are drained in a following bounded batch
 * (1ms apart), so there is no unbounded backlog and no starvation.
 */
internal class EdtFlushPump<T>(
    val label: String,
    private val flushIntervalMs: Int = UI_FLUSH_INTERVAL_MS,
    maxBatch: Int = UI_FLUSH_MAX_BATCH,
    private val metrics: UiWorkMetrics,
    private val weigher: (T) -> Int = { 0 },
    private val sink: (List<T>) -> Unit
) {
    private val queue = CoalescingQueue<T>(
        maxBatch,
        UI_MAX_QUEUED,
        UI_MAX_QUEUED_BYTES,
        weigher
    )
    private val lock = Any()
    private var scheduled = false
    @Volatile
    private var closed = false

    private val timer = Timer(flushIntervalMs) { flushOnEdt() }.apply {
        isRepeats = false
        isCoalesce = true
    }

    fun offer(item: T) {
        if (closed) return
        metrics.offered.incrementAndGet()
        if (!queue.offer(item)) metrics.droppedOverflow.incrementAndGet()
        arm(flushIntervalMs)
    }

    /** Drop queued work (session switch); the pump remains usable. */
    fun discardPending() {
        queue.clear()
        synchronized(lock) {
            scheduled = false
            timer.stop()
        }
    }

    /** Flush immediately on the EDT (explicit refresh/reconnect paths). */
    fun flushNow() {
        if (closed) return
        if (SwingUtilities.isEventDispatchThread()) {
            flushOnEdt()
        } else {
            SwingUtilities.invokeLater { flushOnEdt() }
        }
    }

    fun stop() {
        closed = true
        discardPending()
    }

    val pending: Int get() = queue.size

    private fun arm(delayMs: Int) {
        synchronized(lock) {
            if (closed) return
            if (scheduled && delayMs >= flushIntervalMs) return
            scheduled = true
            timer.stop()
            timer.initialDelay = delayMs.coerceAtLeast(1)
            timer.delay = delayMs.coerceAtLeast(1)
            timer.start()
        }
    }

    private fun flushOnEdt() {
        if (!SwingUtilities.isEventDispatchThread()) {
            SwingUtilities.invokeLater { flushOnEdt() }
            return
        }
        synchronized(lock) { scheduled = false }
        val batch = queue.drain()
        if (batch.isNotEmpty()) {
            metrics.recordFlush(batch.size)
            try {
                sink(batch)
            } catch (t: Throwable) {
                metrics.sinkFailures.incrementAndGet()
                // A rendering exception must never take down the EDT; the
                // next flush retries with fresh data.
                if (t is VirtualMachineError) throw t
            }
        }
        if (!queue.isEmpty) arm(1)
    }
}

/**
 * The follow-vs-detach scroll state machine. PINNED follows appended content
 * only while the user is at the bottom; a user scroll away detaches and new
 * content leaves the viewport alone; returning to the bottom (or an explicit
 * jump/selection) pins again. Pure, so the transitions are testable without a
 * display.
 */
internal class ScrollPin(private val bottomSlackPx: Int = 8) {
    enum class State { PINNED, DETACHED }

    private var state = State.PINNED

    val isPinned: Boolean get() = state == State.PINNED

    val stateForTest: State get() = state

    fun atBottom(value: Int, maximum: Int, extent: Int): Boolean =
        maximum <= extent || value >= maximum - extent - bottomSlackPx

    /** A USER-originated scrollbar move. */
    fun onUserScroll(value: Int, maximum: Int, extent: Int) {
        state = if (atBottom(value, maximum, extent)) State.PINNED else State.DETACHED
    }

    /** An explicit jump/selection re-pins even when the viewport moved off. */
    fun onJump() {
        state = State.PINNED
    }

    fun reset() {
        state = State.PINNED
    }
}

/**
 * Swing adapter for [ScrollPin]: tracks user scrollbar intent (programmatic
 * moves are ignored through [adjusting]) and follows appended content by
 * setting the scrollbar to the bottom ONLY while pinned.
 */
internal class ScrollPinController(
    private val scrollPane: JScrollPane,
    val pin: ScrollPin = ScrollPin()
) {
    private var adjusting = false

    init {
        scrollPane.verticalScrollBar.addAdjustmentListener { event: AdjustmentEvent ->
            if (adjusting) return@addAdjustmentListener
            val bar = event.adjustable as? JScrollBar ?: return@addAdjustmentListener
            pin.onUserScroll(bar.value, bar.maximum, bar.visibleAmount)
        }
    }

    fun followAfterAppend() {
        if (!pin.isPinned) return
        val bar = scrollPane.verticalScrollBar
        adjusting = true
        try {
            bar.value = (bar.maximum - bar.visibleAmount).coerceAtLeast(bar.minimum)
        } finally {
            adjusting = false
        }
    }

    fun jumpToBottom() {
        pin.onJump()
        followAfterAppend()
    }

    fun reset() {
        pin.reset()
        followAfterAppend()
    }
}

/**
 * Batched transcript writes with a maintained budget over the COMPONENT
 * transcript ([TranscriptView]). Callers queue unkeyed notice runs and
 * keyed row ops, then flush once per EDT pass: the flush applies the queued
 * ops in order, one render pass for the whole batch. Row upserts are keyed:
 * an existing row's cell is replaced IN PLACE (its component identity —
 * and any selection/focus inside it — survives), a new row inserts at its
 * ordered anchor, and a row whose range dies to a trim is never re-inserted
 * (content stays exactly-once inside the retained window; the loud
 * "outside the retained window" notice owns the user message).
 *
 * The budget counts each row's plain-text length and newline count; the
 * trim drops the OLDEST cells (never the newest suffix), and lost keys are
 * tombstoned so a later page can never duplicate trimmed content.
 */
internal class TranscriptSink(
    private val view: TranscriptView,
    private val scroll: ScrollPinController?,
    private val metrics: UiWorkMetrics,
    private val maxChars: Int,
    private val maxLines: Int
) {
    /** One queued write: an unkeyed notice run or a keyed row op. */
    private sealed class PendingOp {
        data class Raw(val text: String) : PendingOp()

        data class RowUpsert(
            val key: String,
            val payload: TranscriptRowPayload,
            val beforeKey: String?
        ) : PendingOp()

        data class RowRekey(
            val oldKey: String,
            val newKey: String,
            val payload: TranscriptRowPayload,
            val beforeKey: String?
        ) : PendingOp()
    }

    private val pending = ArrayList<PendingOp>()

    /** Rows whose cells died to a trim: content is never re-inserted. */
    private val lostRows = HashSet<String>()

    private var sysCounter = 0L
    private var partCounter = 0L

    /** The maintained budget (plain-text lengths of every rendered row). */
    var charCount = 0
        private set

    var lineCount = 0
        private set

    val pendingChars: Int
        get() = pending.sumOf {
            when (it) {
                is PendingOp.Raw -> it.text.length
                is PendingOp.RowUpsert -> it.payload.text.length
                is PendingOp.RowRekey -> it.payload.text.length
            }
        }

    /** True while [key] has a live cell (not trimmed, not empty). */
    fun locatedRow(key: String): Boolean = view.contains(key)

    fun append(text: String) {
        if (text.isEmpty()) return
        pending.add(PendingOp.Raw(text))
    }

    fun appendMessage(role: String, text: String, seq: Long) {
        val payload = TranscriptRowPayload(
            kind = when (role) {
                "assistant" -> TranscriptRowPayload.KIND_ASSISTANT
                "user" -> TranscriptRowPayload.KIND_USER
                else -> role
            },
            text = text,
            seq = seq,
            rowId = seq.toString(),
            group = "g:m:$seq"
        )
        upsertRow("mm:$seq", payload, null)
    }

    /**
     * Queue one approval part of the current turn: it interleaves with the
     * text/tool parts by ARRIVAL order and joins their turn group.
     */
    fun appendPart(payload: TranscriptRowPayload) {
        if (payload.text.isEmpty()) return
        pending.add(
            PendingOp.RowUpsert(
                "ap:${partCounter++}",
                payload.copy(seq = partCounter - 1, rowId = "part-${partCounter - 1}"),
                null
            )
        )
    }

    /**
     * Queue a keyed row render: replace the row's cell in place when it is
     * located; otherwise insert [payload] before [beforeKey]'s row (null =
     * the document end). A row lost to a trim is never re-inserted.
     */
    fun upsertRow(key: String, payload: TranscriptRowPayload, beforeKey: String? = null) {
        if (payload.text.isEmpty()) return
        pending.add(PendingOp.RowUpsert(key, payload, beforeKey))
    }

    /**
     * Queue a keyed row move/rename: the [oldKey] cell is removed and
     * [payload] is rendered under [newKey] at the resolved anchor (the
     * provisional-to-durable reconciliation).
     */
    fun rekeyRow(
        oldKey: String,
        newKey: String,
        payload: TranscriptRowPayload,
        beforeKey: String? = null
    ) {
        if (payload.text.isEmpty()) return
        pending.add(PendingOp.RowRekey(oldKey, newKey, payload, beforeKey))
    }

    /**
     * Applies every pending op in queue order (consecutive unkeyed runs
     * become ONE notice cell), follows the viewport only while the user is
     * pinned, then trims once if a bound was crossed.
     */
    fun flush(): Int {
        if (pending.isEmpty()) return 0
        var workChars = 0
        var applied = 0
        var index = 0
        while (index < pending.size) {
            val op = pending[index]
            if (op is PendingOp.Raw) {
                // ONE cell for the consecutive unkeyed run (the notice
                // contract: milestones and refusals never interleave with
                // themselves inside one flush).
                val batch = StringBuilder()
                while (index < pending.size && pending[index] is PendingOp.Raw) {
                    batch.append((pending[index] as PendingOp.Raw).text)
                    index++
                }
                val text = batch.toString()
                view.appendRun(text)
                charCount += text.length
                lineCount += countNewlines(text)
                workChars += text.length
                applied++
                continue
            }
            when (op) {
                is PendingOp.RowUpsert -> {
                    workChars += applyRowUpsert(op.key, op.payload, op.beforeKey)
                    applied++
                }
                is PendingOp.RowRekey -> {
                    workChars += applyRowRekey(
                        op.oldKey, op.newKey, op.payload, op.beforeKey
                    )
                    applied++
                }
                is PendingOp.Raw -> Unit // handled above
            }
            index++
        }
        pending.clear()
        if (applied == 0) return 0
        metrics.transcriptInserts.incrementAndGet()
        metrics.transcriptChars.addAndGet(workChars.toLong())
        scroll?.followAfterAppend()
        // ONE invalidation per flush, after the whole batch landed.
        view.revalidate()
        view.repaint()
        trimIfNeeded()
        return workChars
    }

    /** Replace a located row's cell in place, or insert at the anchor. */
    private fun applyRowUpsert(
        key: String,
        payload: TranscriptRowPayload,
        beforeKey: String?
    ): Int {
        if (key in lostRows) {
            // The row died to a bounded trim: its content lives outside the
            // retained window and must never be re-inserted (exactly-once).
            return 0
        }
        val current = view.payloadOf(key)
        if (current != null) {
            if (current == payload) return 0
            view.upsert(key, payload, beforeKey)
            charCount += payload.text.length - current.text.length
            lineCount += countNewlines(payload.text) - countNewlines(current.text)
            return payload.text.length
        }
        view.upsert(key, payload, beforeKey)
        charCount += payload.text.length
        lineCount += countNewlines(payload.text)
        return payload.text.length
    }

    /** Remove the old key's cell and render [newKey] at its anchor. */
    private fun applyRowRekey(
        oldKey: String,
        newKey: String,
        payload: TranscriptRowPayload,
        beforeKey: String?
    ): Int {
        val oldPayload = view.payloadOf(oldKey)
        if (oldPayload != null) {
            charCount -= oldPayload.text.length
            lineCount -= countNewlines(oldPayload.text)
        }
        lostRows.remove(oldKey)
        view.rekey(oldKey, newKey, payload, beforeKey)
        if (newKey in lostRows) return 0
        charCount += payload.text.length
        lineCount += countNewlines(payload.text)
        return payload.text.length
    }

    fun reset() {
        pending.clear()
        lostRows.clear()
        charCount = 0
        lineCount = 0
        view.clear()
        scroll?.reset()
    }

    /** Trim from the OLDEST cell until the budget holds again. */
    fun trimIfNeeded() {
        if (charCount <= maxChars && lineCount <= maxLines) return
        var trimmed = 0
        while (charCount > maxChars || lineCount > maxLines) {
            val removed = view.removeOldest() ?: break
            charCount -= removed.payload.text.length
            lineCount -= countNewlines(removed.payload.text)
            lostRows.add(removed.key)
            trimmed++
        }
        if (trimmed > 0) {
            charCount = charCount.coerceAtLeast(0)
            lineCount = lineCount.coerceAtLeast(0)
            metrics.transcriptTrims.incrementAndGet()
            metrics.transcriptInserts.incrementAndGet()
            scroll?.followAfterAppend()
            view.revalidate()
            view.repaint()
        }
    }

    private fun countNewlines(text: String): Int {
        var count = 0
        for (index in text.indices) if (text[index] == '\n') count++
        return count
    }
}

/**
 * Bounded, thread-safe formatting caches. Money renders repeatedly (every
 * refresh, every panel); formatting a BigInteger with division is pure work,
 * so the canonical text is memoized by the exact value (and the currency for
 * the display form). The cache is an access-order LRU with a hard bound, so a
 * hostile stream of distinct amounts cannot grow it without limit.
 */
internal object FormattedValues {
    const val MAX_ENTRIES = 4096

    private data class MoneyKey(val amount: BigInteger, val currency: String)

    private class BoundedCache<K, V>(private val max: Int) {
        private val map = object : LinkedHashMap<K, V>(64, 0.75f, true) {
            override fun removeEldestEntry(eldest: MutableMap.MutableEntry<K, V>?): Boolean =
                size > max
        }
        var hits = 0L
        var misses = 0L
        var evictions = 0L

        fun get(key: K): V? {
            val value = map[key]
            if (value == null) misses++ else hits++
            return value
        }

        fun put(key: K, value: V) {
            if (!map.containsKey(key) && map.size >= max) evictions++
            map[key] = value
        }

        val size: Int get() = map.size

        fun clear() {
            map.clear()
            hits = 0L
            misses = 0L
            evictions = 0L
        }
    }

    private val microCache = BoundedCache<BigInteger, String>(MAX_ENTRIES)
    private val usdCache = BoundedCache<MoneyKey, String>(MAX_ENTRIES)
    private val percentCache = BoundedCache<Pair<BigInteger, BigInteger>, Int>(MAX_ENTRIES)

    @Synchronized
    fun micro(value: BigInteger): String =
        microCache.get(value) ?: MicroMoney.microText(value).also { microCache.put(value, it) }

    @Synchronized
    fun usd(value: BigInteger, currency: String = "USD"): String {
        val key = MoneyKey(value, currency)
        return usdCache.get(key) ?: MicroMoney.usdText(value).also { usdCache.put(key, it) }
    }

    @Synchronized
    fun currency(value: BigInteger, currency: String = "USD"): String =
        if (currency == "USD") "$" + usd(value) else usd(value, currency) + " " + currency

    /** Bounded percentage of `observed / limit` (null when the limit <= 0). */
    @Synchronized
    fun percent(observed: BigInteger, limit: BigInteger): Int? {
        if (limit.signum() <= 0) return null
        val key = Pair(observed, limit)
        percentCache.get(key)?.let { return it }
        val percent = observed.multiply(BigInteger.valueOf(100))
            .divide(limit)
            .toInt()
            .coerceIn(0, 100)
        percentCache.put(key, percent)
        return percent
    }

    @Synchronized
    fun stats(): FormatCacheStats = FormatCacheStats(
        microEntries = microCache.size,
        usdEntries = usdCache.size,
        percentEntries = percentCache.size,
        hits = microCache.hits + usdCache.hits + percentCache.hits,
        misses = microCache.misses + usdCache.misses + percentCache.misses,
        evictions = microCache.evictions + usdCache.evictions + percentCache.evictions
    )

    @Synchronized
    fun clear() {
        microCache.clear()
        usdCache.clear()
        percentCache.clear()
    }
}

internal data class FormatCacheStats(
    val microEntries: Int,
    val usdEntries: Int,
    val percentEntries: Int,
    val hits: Long,
    val misses: Long,
    val evictions: Long
)

/**
 * Granular list synchronization keyed by a stable id. An identical snapshot
 * is a no-op; changed rows are replaced in place; order changes reuse the
 * remaining objects. This is the replacement for clear()+addAll() model
 * resets (which drop selection, scroll and expansion on every refresh).
 */
internal object KeyedListSync {
    data class Result(val inserted: Int, val updated: Int, val removed: Int, val changed: Boolean)

    fun <T> sync(
        model: DefaultListModel<T>,
        desired: List<T>,
        key: (T) -> String
    ): Result {
        if (sameSnapshot(model, desired)) return Result(0, 0, 0, false)
        var inserted = 0
        var updated = 0
        var removed = 0
        val desiredKeys = desired.mapTo(HashSet()) { key(it) }
        var index = 0
        while (index < model.size()) {
            if (key(model.getElementAt(index)) !in desiredKeys) {
                model.remove(index)
                removed++
            } else {
                index++
            }
        }
        for (target in desired.indices) {
            val wanted = desired[target]
            val wantedKey = key(wanted)
            if (target < model.size() && key(model.getElementAt(target)) == wantedKey) {
                if (model.getElementAt(target) != wanted) {
                    model.set(target, wanted)
                    updated++
                }
                continue
            }
            var found = -1
            for (candidate in target until model.size()) {
                if (key(model.getElementAt(candidate)) == wantedKey) {
                    found = candidate
                    break
                }
            }
            if (found >= 0) {
                val existing = model.getElementAt(found)
                model.remove(found)
                model.add(target, existing)
                inserted++
                if (existing != wanted) {
                    model.set(target, wanted)
                    updated++
                }
            } else {
                model.add(target, wanted)
                inserted++
            }
        }
        while (model.size() > desired.size) {
            model.remove(model.size() - 1)
            removed++
        }
        return Result(inserted, updated, removed, true)
    }

    @Suppress("UNCHECKED_CAST")
    fun <T> syncCombo(
        model: DefaultComboBoxModel<T>,
        desired: List<T>,
        key: (T) -> String
    ): Result {
        val previousItem = model.selectedItem as? T
        val previousKey = previousItem?.let { key(it) }
        if (sameSnapshot(model, desired)) {
            if (previousKey != null) selectCombo(model, desired, key, previousKey)
            return Result(0, 0, 0, false)
        }
        var inserted = 0
        var updated = 0
        var removed = 0
        val desiredKeys = desired.mapTo(HashSet()) { key(it) }
        var index = 0
        while (index < model.size) {
            if (key(model.getElementAt(index)) !in desiredKeys) {
                model.removeElementAt(index)
                removed++
            } else {
                index++
            }
        }
        for (target in desired.indices) {
            val wanted = desired[target]
            val wantedKey = key(wanted)
            if (target < model.size && key(model.getElementAt(target)) == wantedKey) {
                if (model.getElementAt(target) != wanted) {
                    model.removeElementAt(target)
                    model.insertElementAt(wanted, target)
                    updated++
                }
                continue
            }
            var found = -1
            for (candidate in target until model.size) {
                if (key(model.getElementAt(candidate)) == wantedKey) {
                    found = candidate
                    break
                }
            }
            if (found >= 0) {
                val existing = model.getElementAt(found)
                model.removeElementAt(found)
                model.insertElementAt(existing, target)
                inserted++
                if (existing != wanted) {
                    model.removeElementAt(target)
                    model.insertElementAt(wanted, target)
                    updated++
                }
            } else {
                model.insertElementAt(wanted, target)
                inserted++
            }
        }
        while (model.size > desired.size) {
            model.removeElementAt(model.size - 1)
            removed++
        }
        previousKey?.let { selectCombo(model, desired, key, it) }
        return Result(inserted, updated, removed, true)
    }

    private fun <T> sameSnapshot(model: DefaultListModel<T>, desired: List<T>): Boolean {
        if (model.size() != desired.size) return false
        for (index in desired.indices) {
            if (model.getElementAt(index) != desired[index]) return false
        }
        return true
    }

    private fun <T> sameSnapshot(model: DefaultComboBoxModel<T>, desired: List<T>): Boolean {
        if (model.size != desired.size) return false
        for (index in desired.indices) {
            if (model.getElementAt(index) != desired[index]) return false
        }
        return true
    }

    private fun <T> selectCombo(
        model: DefaultComboBoxModel<T>,
        desired: List<T>,
        key: (T) -> String,
        wantedKey: String
    ) {
        for (index in desired.indices) {
            if (key(desired[index]) == wantedKey) {
                model.selectedItem = desired[index]
                return
            }
        }
    }
}

/**
 * Per-session teardown. UI resources bound to a session (queued deltas,
 * message offsets, view state, observers) register their close here; a
 * session switch or shutdown disposes the scope exactly once.
 */
internal class SessionUiScope(private val metrics: UiWorkMetrics? = null) {
    private val disposers = ArrayList<() -> Unit>()
    private var disposed = false

    fun onDispose(block: () -> Unit) {
        synchronized(this) {
            if (disposed) {
                block()
                return
            }
            disposers.add(block)
        }
    }

    fun disposeAll() {
        val local: List<() -> Unit>
        synchronized(this) {
            if (disposed) return
            disposed = true
            local = ArrayList(disposers)
            disposers.clear()
        }
        for (close in local) {
            try {
                close()
            } catch (ignored: Exception) {
                // Teardown is best-effort per callback; the rest must run.
            }
        }
        metrics?.sessionScopesDisposed?.incrementAndGet()
    }

    val isDisposed: Boolean
        get() = synchronized(this) { disposed }
}

/** Accessible name/description in one call (no visual/layout effect). */
internal fun <T : JComponent> T.a11y(name: String, description: String? = null): T {
    accessibleContext.accessibleName = name
    if (!description.isNullOrBlank()) {
        accessibleContext.accessibleDescription = description
    }
    return this
}

/**
 * Name the Metal LAF's scrollbar arrow buttons. They are real interactive
 * controls in the component tree (a screen reader reaches them), but the LAF
 * creates them without an accessible name, so every scrollable screen failed
 * the Linux headful audit. Naming them is a product accessibility fix, not a
 * test exemption: direction is the only thing the control does.
 */
internal fun installScrollButtonAccessibility(component: java.awt.Component) {
    if (component is javax.swing.plaf.metal.MetalScrollButton) {
        val label = when (component.direction) {
            javax.swing.SwingConstants.NORTH -> "Scroll up"
            javax.swing.SwingConstants.SOUTH -> "Scroll down"
            javax.swing.SwingConstants.EAST -> "Scroll right"
            javax.swing.SwingConstants.WEST -> "Scroll left"
            else -> "Scroll"
        }
        component.a11y(label, "Scrolls the surrounding view.")
    }
    if (component is java.awt.Container) {
        for (child in component.components) installScrollButtonAccessibility(child)
    }
}

/**
 * The Enter/Space activation stroke for lists and choosers: the focused
 * component activates its primary action with the keyboard, exactly like a
 * click. Installed with WHEN_FOCUSED so typing in a text field is untouched.
 */
internal fun installActivateKey(component: JComponent, actionName: String, block: () -> Unit) {
    component.inputMap.put(
        KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ENTER, 0),
        actionName
    )
    component.actionMap.put(actionName, object : AbstractAction() {
        override fun actionPerformed(event: ActionEvent?) {
            block()
        }
    })
}
