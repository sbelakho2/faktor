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
import javax.swing.text.JTextComponent

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
 * Batched transcript writes with a maintained budget. Callers append segments
 * (optionally keyed by a durable message seq) and flush once per EDT pass:
 * one `Document.insertString` for the whole batch. Trimming happens only when
 * a bound is crossed, never per delta, and clears stale offsets loudly.
 */
internal class TranscriptSink(
    private val area: JTextComponent,
    private val scroll: ScrollPinController?,
    private val metrics: UiWorkMetrics,
    private val maxChars: Int,
    private val maxLines: Int
) {
    private data class Segment(val text: String, val seq: Long?)

    private val pending = ArrayList<Segment>()
    private val offsets = LinkedHashMap<Long, Pair<Int, Int>>()
    private var charCount = 0
    private var lineCount = 0
    private var initialized = false

    /** Durable message offsets, rebuilt only after a trim. */
    val messageOffsets: Map<Long, Pair<Int, Int>>
        get() = offsets

    val pendingChars: Int
        get() = pending.sumOf { it.text.length }

    fun offsetOf(seq: Long): Pair<Int, Int>? = offsets[seq]

    fun append(text: String, seq: Long? = null) {
        if (text.isEmpty()) return
        pending.add(Segment(text, seq))
    }

    fun appendMessage(role: String, text: String, seq: Long) {
        append("$role: $text\n", seq)
    }

    /**
     * Writes every pending segment as ONE document insert, records message
     * offsets against the document positions, follows the viewport only while
     * the user is pinned, then trims once if the budget was crossed.
     */
    fun flush(): Int {
        initialize()
        if (pending.isEmpty()) return 0
        val batch = StringBuilder()
        val positions = ArrayList<Triple<Long, Int, Int>>()
        var offset = area.document.length
        for (segment in pending) {
            val start = offset
            batch.append(segment.text)
            offset += segment.text.length
            segment.seq?.let { positions.add(Triple(it, start, offset)) }
        }
        val added = batch.length
        pending.clear()
        if (added == 0) return 0
        area.document.insertString(area.document.length, batch.toString(), null)
        for ((seq, start, end) in positions) offsets[seq] = Pair(start, end)
        charCount += added
        lineCount += countLines(batch)
        metrics.transcriptInserts.incrementAndGet()
        metrics.transcriptChars.addAndGet(added.toLong())
        scroll?.followAfterAppend()
        // ONE invalidation per flush, after the whole batch landed.
        area.revalidate()
        area.repaint()
        trimIfNeeded()
        return added
    }

    fun reset() {
        pending.clear()
        offsets.clear()
        charCount = 0
        lineCount = 0
        initialized = true
        area.text = ""
        scroll?.reset()
    }

    fun trimIfNeeded() {
        initialize()
        if (charCount <= maxChars && lineCount <= maxLines) return
        val text = area.text
        val lines = text.split('\n')
        val keptLines = if (lines.size > maxLines) lines.takeLast(maxLines) else lines
        var kept = keptLines.joinToString("\n")
        if (kept.length > maxChars) kept = safeTakeLast(kept, maxChars)
        area.text = kept
        charCount = kept.length
        lineCount = kept.count { it == '\n' }
        // Offsets no longer map onto the rebuilt text; message navigation
        // falls back to the loud "outside the retained window" notice.
        offsets.clear()
        metrics.transcriptTrims.incrementAndGet()
        metrics.transcriptInserts.incrementAndGet()
        scroll?.followAfterAppend()
        area.revalidate()
        area.repaint()
    }

    private fun initialize() {
        if (initialized) return
        initialized = true
        charCount = area.document.length
        lineCount = area.text.count { it == '\n' }
    }

    private fun countLines(batch: CharSequence): Int {
        var count = 0
        for (i in 0 until batch.length) if (batch[i] == '\n') count++
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
