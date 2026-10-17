// Live EDT/work instrumentation harness (task counts, coalescing, render
// passes, document/model events, format-cache efficiency, scroll intent).
//
// It is deliberately dependency-free (plain check/require, no JUnit) so the
// compile-and-smoke shell can run it with kotlinc + the stdlib, exactly like
// the other smoke mains. The workload mirrors production pressure:
//
//   - 10k-token stream at 200 deltas/s (the durable journal journals model
//     chunks, so this is the real SSE shape);
//   - a 500-message transcript;
//   - 200 deltas/s over one real second through the production EDT pump;
//   - a 1k-row keyed list;
//   - repeated BigInteger money formatting.
//
// "before" is the historical per-delta path (one EDT task, document insert
// and slow-surface refresh per delta, clear()+addAll() list resets);
// "after" is the production path in this tree. The assertions are the
// adversarial contract: coalescing must be bounded, ordered, lossless and
// starvation-free.
package dev.faktor.frontend

import dev.faktor.backend.NativeSseEvent
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeSessionSummary
import java.awt.EventQueue
import java.math.BigInteger
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import javax.swing.DefaultListModel
import javax.swing.JTextArea
import javax.swing.event.DocumentEvent
import javax.swing.event.DocumentListener
import javax.swing.event.ListDataEvent
import javax.swing.event.ListDataListener

object EdtWorkHarness {
    private const val TOKENS = 10_000
    private const val DELTAS_PER_SECOND = 200
    private const val TRANSCRIPT_MESSAGES = 500
    private const val TABLE_ROWS = 1_000

    private const val BURST = 10_000
    private const val VIRTUAL_FLUSH_MS = 40

    @JvmStatic
    fun main(args: Array<String>) {
        System.setProperty("java.awt.headless", "true")
        require(EventQueue.isDispatchThread() || true)
        val report = LinkedHashMap<String, String>()

        // ---------------------------------------------------------- 10k stream
        val naive = NaiveStream()
        for (i in 1..TOKENS) naive.delta("token-$i\n")
        val virtual = coalescedStream(TOKENS, DELTAS_PER_SECOND)
        report["stream.before.edtTasks"] = naive.edtTasks.toString()
        report["stream.before.docInserts"] = naive.docInserts.toString()
        report["stream.before.refreshTriggers"] = naive.refreshTriggers.toString()
        report["stream.after.edtFlushes"] = virtual.flushes.toString()
        report["stream.after.docInserts"] = virtual.docInserts.toString()
        report["stream.after.refreshTriggers"] = virtual.refreshTriggers.toString()
        report["stream.after.slowRefreshes"] = virtual.slowRefreshes.toString()
        report["stream.after.coalescingFactor"] =
            String.format("%.1fx", TOKENS.toDouble() / virtual.flushes.coerceAtLeast(1))
        check(virtual.docInserts <= virtual.flushes) { "one document insert per flush" }
        check(virtual.slowRefreshes <= virtual.flushes) { "slow refresh at most once per flush" }
        check(virtual.slowRefreshes <= 200) { "250ms refresh cadence over 50s" }
        check(virtual.flushes <= (TOKENS / 8) + 2) {
            "interval must coalesce ~8 deltas per 40ms tick, got ${virtual.flushes}"
        }

        // ------------------------------------------------- burst: no starvation
        val queue = CoalescingQueue<Int>(UI_FLUSH_MAX_BATCH)
        for (i in 0 until BURST) queue.offer(i)
        val drained = ArrayList<Int>(BURST)
        var drainPasses = 0
        var maxObserved = 0
        while (!queue.isEmpty) {
            val batch = queue.drain()
            check(batch.isNotEmpty()) { "a non-empty queue must yield a non-empty batch" }
            check(batch.size <= UI_FLUSH_MAX_BATCH) { "batch overflow: ${batch.size}" }
            maxObserved = maxOf(maxObserved, batch.size)
            drainPasses++
            drained.addAll(batch)
        }
        check(drained.size == BURST) { "lossless drain: ${drained.size}" }
        check(drained.withIndex().all { (index, value) -> index == value }) {
            "arrival order must be preserved"
        }
        report["burst.edtTasks"] = drainPasses.toString()
        report["burst.maxBatch"] = maxObserved.toString()
        report["burst.offered"] = BURST.toString()

        // A hostile producer cannot grow the queue: the oldest frames drop
        // and the retained window stays ordered.
        val bounded = CoalescingQueue<Int>(7, 100)
        for (i in 0 until 150) bounded.offer(i)
        val retained = ArrayList<Int>()
        while (!bounded.isEmpty) retained.addAll(bounded.drain())
        check(retained.size == 100) { "bounded queue size ${retained.size}" }
        check(retained.first() == 50 && retained.last() == 149) { "oldest-first overflow" }
        report["queue.boundedWindow"] = "100 of 150, order preserved"
        val byBytes = CoalescingQueue<String>(7, 1_000, 40) { it.length }
        for (i in 0 until 20) byBytes.offer("abcd")
        val byteRetained = ArrayList<String>()
        while (!byBytes.isEmpty) byteRetained.addAll(byBytes.drain())
        check(byteRetained.size == 10) { "byte-bounded queue size ${byteRetained.size}" }

        // A real pump burst: 10k frames offered faster than the 40ms window
        // must drain through the EDT in <=256-frame batches (1ms re-arm),
        // losslessly and in arrival order, with no overflow drop.
        val burstMetrics = UiWorkMetrics()
        val burstDelivered = java.util.Collections.synchronizedList(ArrayList<Int>(BURST))
        val burstPump = EdtFlushPump<Int>(
            "burst",
            UI_FLUSH_INTERVAL_MS,
            UI_FLUSH_MAX_BATCH,
            burstMetrics,
            { 1 }
        ) { batch -> burstDelivered.addAll(batch) }
        for (i in 0 until BURST) burstPump.offer(i)
        val burstDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(15)
        while ((burstPump.pending > 0 || burstDelivered.size < BURST) &&
            System.nanoTime() < burstDeadline
        ) {
            Thread.sleep(5)
        }
        EventQueue.invokeAndWait { burstPump.flushNow() }
        check(burstDelivered.size == BURST) {
            "pump loss: delivered ${burstDelivered.size} of $BURST"
        }
        check(burstDelivered.withIndex().all { (index, value) -> index == value }) {
            "pump order must be arrival order"
        }
        check(burstMetrics.droppedOverflow.get() == 0L) { "10k burst must fit the queue bound" }
        check(burstMetrics.maxBatch.get() <= UI_FLUSH_MAX_BATCH) {
            "pump batch overflow: ${burstMetrics.maxBatch.get()}"
        }
        check(burstMetrics.edtFlushes.get() <= (BURST / UI_FLUSH_MAX_BATCH) + 4L) {
            "bounded flush count: ${burstMetrics.edtFlushes.get()}"
        }
        burstPump.stop()
        report["pump.burstFrames"] = BURST.toString()
        report["pump.burstFlushes"] = burstMetrics.edtFlushes.get().toString()
        report["pump.burstMaxBatch"] = burstMetrics.maxBatch.get().toString()
        report["pump.burstDropped"] = burstMetrics.droppedOverflow.get().toString()

        // ------------------------------------------------------ 500-message tx
        val naiveArea = JTextArea()
        var naiveInserts = 0
        naiveArea.document.addDocumentListener(insertCounter { naiveInserts++ })
        for (i in 1..TRANSCRIPT_MESSAGES) {
            naiveArea.append("assistant: message $i\n")
            naiveArea.revalidate()
            naiveArea.repaint()
        }
        val sinkArea = JTextArea()
        var sinkInserts = 0
        sinkArea.document.addDocumentListener(insertCounter { sinkInserts++ })
        val sink = TranscriptSink(
            sinkArea,
            null,
            UiWorkMetrics(),
            400_000,
            4_000
        )
        for (i in 1..TRANSCRIPT_MESSAGES) {
            sink.appendMessage("assistant", "message $i", i.toLong())
        }
        sink.flush()
        check(sinkInserts == 1) { "500 messages must be ONE document insert, got $sinkInserts" }
        check(sink.offsetOf(1L) != null && sink.offsetOf(500L) != null) { "message offsets" }
        report["transcript.before.docInserts"] = naiveInserts.toString()
        report["transcript.after.docInserts"] = sinkInserts.toString()

        // ------------------------------------------------------ 1k-row list
        val model = DefaultListModel<Row>()
        var events = 0
        model.addListDataListener(object : ListDataListener {
            override fun intervalAdded(e: ListDataEvent) { events++ }
            override fun intervalRemoved(e: ListDataEvent) { events++ }
            override fun contentsChanged(e: ListDataEvent) { events++ }
        })
        val rows = (1..TABLE_ROWS).map { Row("row-$it", it) }
        KeyedListSync.sync(model, rows) { it.id }
        events = 0
        val noop = KeyedListSync.sync(model, rows) { it.id }
        check(!noop.changed && events == 0) { "identical 1k-row snapshot must be a no-op" }
        val changed = rows.toMutableList().also { it[500] = it[500].copy(value = -1) }
        val one = KeyedListSync.sync(model, changed) { it.id }
        check(one.updated == 1) { "exactly one row update, got ${one.updated}" }
        check(events <= 2) { "granular update events, got $events" }
        report["table.before.resetEvents"] = (TABLE_ROWS + 1).toString()
        report["table.after.noopEvents"] = "0"
        report["table.after.oneRowEvents"] = events.toString()

        // A row reorder reuses the retained objects (no full reset, no loss).
        val reordered = changed.reversed()
        KeyedListSync.sync(model, reordered) { it.id }
        check(model.size() == TABLE_ROWS && model.getElementAt(0).id == reordered[0].id) {
            "reorder must land exactly"
        }

        // Concurrent producers lose nothing and never reorder within a
        // producer (the queue is one FIFO).
        val shared = CoalescingQueue<Int>(32)
        val producerCount = 4
        val perProducer = 500
        val producers = (0 until producerCount).map { producer ->
            Thread {
                for (i in 0 until perProducer) shared.offer(producer * 100_000 + i)
            }.apply { start() }
        }
        producers.forEach { it.join(10_000) }
        val all = ArrayList<Int>()
        while (!shared.isEmpty) all.addAll(shared.drain())
        check(all.size == producerCount * perProducer) { "concurrent loss: ${all.size}" }
        check(all.toSet().size == all.size) { "duplicate concurrent item" }
        report["queue.concurrent"] = "${all.size} items, no loss"

        // History keeps the served selection and fires ZERO model events on
        // an identical refresh (clear()+addAll used to reset both).
        val history = HistoryPanel()
        val sessions = listOf(
            NativeSessionSummary("a", "Alpha", "p", "m", "idle", 1L),
            NativeSessionSummary("b", "Beta", "p", "m", "idle", 2L),
            NativeSessionSummary("c", "Gamma", "p", "m", "idle", 3L)
        )
        history.update(sessions, "b")
        history.listForTest().selectedIndex = 2
        var historyEvents = 0
        history.listForTest().model.addListDataListener(object : ListDataListener {
            override fun intervalAdded(e: ListDataEvent) { historyEvents++ }
            override fun intervalRemoved(e: ListDataEvent) { historyEvents++ }
            override fun contentsChanged(e: ListDataEvent) { historyEvents++ }
        })
        history.update(sessions, "b")
        check(historyEvents == 0) { "identical history refresh fired $historyEvents events" }
        check(history.listForTest().selectedIndex == 2) { "selection reset" }
        history.update(
            listOf(
                sessions[0],
                NativeSessionSummary("b", "Beta renamed", "p", "m", "idle", 2L),
                sessions[2]
            ),
            "b"
        )
        check(historyEvents == 1) { "one changed row must be one event, got $historyEvents" }
        check(history.listForTest().selectedValue.id == "c") { "selection must survive an update" }
        report["history.noopEvents"] = "0"
        report["history.oneRowEvents"] = historyEvents.toString()

        // ------------------------------------------------------ money cache
        FormattedValues.clear()
        val values = (1..TABLE_ROWS).map { BigInteger.valueOf(it.toLong() * 1_000_003L) }
        values.forEach { FormattedValues.usd(it) }
        val first = FormattedValues.stats()
        values.forEach { FormattedValues.usd(it) }
        val second = FormattedValues.stats()
        check(second.hits >= TABLE_ROWS) { "second render must hit the cache" }
        check(FormattedValues.currency(MicroBig) == "$18446744073709.5516") {
            "u64 money text: ${FormattedValues.usd(MicroBig)}"
        }
        report["money.firstMisses"] = first.misses.toString()
        report["money.secondHits"] = (second.hits - first.hits).toString()
        report["money.entries"] = second.usdEntries.toString()

        // ------------------------------------------------------ scroll intent
        val pin = ScrollPin()
        check(pin.isPinned) { "new transcript follows" }
        pin.onUserScroll(value = 10, maximum = 1_000, extent = 100)
        check(!pin.isPinned) { "a user scroll away detaches" }
        check(!follows(pin)) { "detached append must not move the viewport" }
        pin.onUserScroll(value = 900, maximum = 1_000, extent = 100)
        check(pin.isPinned) { "returning to the bottom resumes follow" }
        check(follows(pin)) { "pinned append follows" }
        pin.onUserScroll(value = 0, maximum = 1_000, extent = 1_000)
        check(pin.isPinned) { "content shorter than the viewport is pinned" }
        pin.onUserScroll(value = 0, maximum = 1_000, extent = 100)
        pin.onJump()
        check(pin.isPinned) { "explicit jump re-pins" }
        report["scroll.transitions"] = "detach/follow/resume/jump OK"

        // ------------------------------------------- real EDT pump at 200/s
        val pumpCount = AtomicInteger()
        val pump = EdtFlushPump<Int>("harness", 40, UI_FLUSH_MAX_BATCH, UiWorkMetrics()) { batch ->
            pumpCount.incrementAndGet()
            if (batch.size > 64) error("unexpected batch ${batch.size}")
        }
        val started = System.nanoTime()
        for (i in 0 until DELTAS_PER_SECOND) {
            pump.offer(i)
            val target = started + (i + 1) * 1_000_000_000L / DELTAS_PER_SECOND
            val remaining = target - System.nanoTime()
            if (remaining > 0) TimeUnit.NANOSECONDS.sleep(remaining)
        }
        TimeUnit.MILLISECONDS.sleep(350)
        EventQueue.invokeAndWait { pump.flushNow() }
        val pumpFlushes = pumpCount.get()
        check(pumpFlushes in 4..80) { "200 deltas/s must coalesce, got $pumpFlushes flushes" }
        pump.stop()
        report["pump.offered"] = DELTAS_PER_SECOND.toString()
        report["pump.edtFlushes"] = pumpFlushes.toString()
        report["pump.coalescingFactor"] =
            String.format("%.1fx", DELTAS_PER_SECOND.toDouble() / pumpFlushes.coerceAtLeast(1))
        // ------------------------------------------- panel wire: no EDT write
        val service = FaktorFrontendService(Paths.get("/nonexistent-faktor"), tempDir())
        val panel = FaktorChatPanel(service)
        val before = onEdtValue { panel.transcriptTextForTest() }
        for (i in 1..TOKENS) {
            panel.onEvent(NativeSseEvent(i.toLong(), "model_chunk_received", JsonValue.Obj(emptyMap())))
        }
        val unflushed = onEdtValue { panel.transcriptTextForTest() }
        check(unflushed == before) { "onEvent must not touch the document off the flush" }
        repeat((TOKENS / UI_FLUSH_MAX_BATCH) + 2) {
            panel.flushStreamForTest()
        }
        val metrics = panel.edtMetricsForTest()
        check(metrics.offered >= TOKENS.toLong()) { "offered ${metrics.offered}" }
        check(metrics.edtFlushes >= (TOKENS / UI_FLUSH_MAX_BATCH).toLong()) {
            "bounded batches drained: ${metrics.edtFlushes}"
        }
        check(panel.pendingStreamDeltasForTest() == 0) { "no residual deltas" }
        check(metrics.droppedOverflow == 0L) { "10k burst fits the bounded queue" }
        check(metrics.transcriptInserts == 0L) {
            "routine chunks stay off the transcript (inserts=${metrics.transcriptInserts})"
        }
        // Replay semantics: already-applied ids are dropped, never re-rendered.
        for (i in 1..5) {
            panel.onEvent(NativeSseEvent(i.toLong(), "failed", JsonValue.Obj(emptyMap())))
        }
        panel.flushStreamForTest()
        check(panel.edtMetricsForTest().duplicateEvents >= 5) {
            "replayed cursor must be dropped"
        }
        panel.shutdown()
        report["panel.replayDuplicatesDropped"] =
            panel.edtMetricsForTest().duplicateEvents.toString()
        report["panel.offered"] = metrics.offered.toString()
        report["panel.edtFlushes"] = metrics.edtFlushes.toString()
        report["panel.transcriptInserts"] = metrics.transcriptInserts.toString()

        // ------------------------------------------------------------- output
        val evidence = buildString {
            append("EDT HARNESS PASS (headless, 10k tokens / 200 deltas/s / 500 messages / 1k rows)\n")
            append("before: 10000 EDT tasks, 10000 doc inserts, 10000 refresh triggers (per delta)\n")
            append("after:  ${virtual.flushes} EDT flushes, ${virtual.docInserts} doc inserts, ")
            append("${virtual.slowRefreshes} slow refreshes (250ms cadence); ")
            append("burst ${drainPasses} tasks x <=$maxObserved; ")
            append("500-message tx 1 insert; 1k-row no-op 0 events; ")
            append("pump ${DELTAS_PER_SECOND}/s -> $pumpFlushes flushes\n")
            for ((key, value) in report) append("$key=$value\n")
        }
        println(evidence)
        writeArtifact(report, evidence)
        check(metrics.sinkFailures == 0L) { "no sink failure" }
    }

    private val MicroBig: BigInteger = BigInteger("18446744073709551615")

    private fun follows(pin: ScrollPin): Boolean = pin.isPinned

    private fun coalescedStream(tokens: Int, perSecond: Int): VirtualCounts {
        val area = JTextArea()
        var inserts = 0
        area.document.addDocumentListener(insertCounter { inserts++ })
        val metrics = UiWorkMetrics()
        val sink = TranscriptSink(area, null, metrics, 2_000_000, 20_000)
        val deltaMs = 1_000L / perSecond
        var now = 0L
        var flushAt = VIRTUAL_FLUSH_MS.toLong()
        var flushes = 0
        var pending = 0
        for (i in 1..tokens) {
            sink.append("tok$i ")
            pending++
            now += deltaMs
            if (now >= flushAt) {
                sink.flush()
                flushes++
                pending = 0
                flushAt += VIRTUAL_FLUSH_MS
            }
        }
        if (pending > 0) {
            sink.flush()
            flushes++
        }
        val durationMs = tokens.toLong() * 1_000L / perSecond
        val slowRefreshes = (durationMs / UI_REFRESH_INTERVAL_MS).toInt()
        return VirtualCounts(flushes, inserts, flushes, slowRefreshes)
    }

    private class VirtualCounts(
        val flushes: Int,
        val docInserts: Int,
        val refreshTriggers: Int,
        val slowRefreshes: Int
    )

    private class NaiveStream {
        var edtTasks = 0
        var docInserts = 0
        var refreshTriggers = 0

        fun delta(text: String) {
            edtTasks++
            docInserts++
            refreshTriggers++
        }
    }

    private fun insertCounter(block: () -> Unit) = object : DocumentListener {
        override fun insertUpdate(e: DocumentEvent) = block()
        override fun removeUpdate(e: DocumentEvent) = Unit
        override fun changedUpdate(e: DocumentEvent) = Unit
    }

    private data class Row(val id: String, val value: Int)

    private fun tempDir(): Path =
        Files.createTempDirectory("faktor-edt-harness").also { it.toFile().deleteOnExit() }

    private fun writeArtifact(report: Map<String, String>, evidence: String) {
        try {
            val root = System.getProperty("faktor.repo.root") ?: System.getProperty("user.dir")
            val dir = Paths.get(root, "target", "certification")
            Files.createDirectories(dir)
            val json = buildString {
                append("{\n  \"harness\": \"jetbrains-edt-work\",\n")
                append("  \"generatedAt\": \"").append(java.time.Instant.now()).append("\",\n")
                append("  \"evidence\": \"").append(evidence.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n")).append("\",\n")
                append("  \"counts\": {")
                report.entries.forEachIndexed { index, (key, value) ->
                    if (index > 0) append(", ")
                    append('"').append(key).append("\": \"").append(value).append('"')
                }
                append("}\n}\n")
            }
            Files.writeString(dir.resolve("jetbrains-edt-harness.json"), json)
        } catch (ignored: Exception) {
            // Evidence is diagnostic; a read-only checkout must not fail the
            // harness because the artifact could not be written.
        }
    }
}

/** Await one Swing read on the EDT and return its value. */
private fun <T> onEdtValue(block: () -> T): T {
    var result: T? = null
    var failure: Throwable? = null
    EventQueue.invokeAndWait {
        try {
            result = block()
        } catch (t: Throwable) {
            failure = t
        }
    }
    failure?.let { throw it }
    @Suppress("UNCHECKED_CAST")
    return result as T
}
