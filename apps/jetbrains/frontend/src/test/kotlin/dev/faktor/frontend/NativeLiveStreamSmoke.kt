// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
//
// Adversarial live-output / turn-reducer smoke for the JetBrains frontend.
//
// The ORACLE is the Faktor Native Protocol v1 live contract (mirrored from
// `apps/vscode/src/eventStream.ts` + `turnState.ts` and the daemon route
// `crates/server/src/native/live_output.rs`), plus EXTERNAL fixtures: a fake
// live SSE daemon and a canned transcript daemon. The SUT never grades
// itself; every case tries to break the boundary:
//
//  - live frames advance the transcript BEFORE any durable flush;
//  - duplicate/replayed frames are dropped; an omitted range (offset or
//    revision jump) requests an explicit resync and the authoritative
//    snapshot reconciles byte-identically;
//  - frames from a foreign server instance are dropped (daemon restart);
//  - UTF-8 byte offsets never mix with UTF-16 indices (CJK/emoji);
//  - a saved/durable page never overwrites a newer live suffix and durable
//    divergence wins;
//  - Stop targets the accepted operation id (a queued follow-up is not);
//  - a queued follow-up renders `Queued` while an earlier turn streams;
//  - a reconnect snapshot reconciles the visible text exactly once;
//  - the reducer state matrix (including sticky terminals and pruning).
//
// Isolation: every case owns its temp data dir/workspace and fake daemon; no
// developer ~/.faktor state is read or written.
package dev.faktor.frontend

import dev.faktor.backend.BackendConnection
import dev.faktor.backend.LiveChunkFrame
import dev.faktor.backend.LiveOutputMessage
import dev.faktor.backend.LiveSnapshot
import dev.faktor.backend.LiveSnapshotPart
import dev.faktor.backend.LiveTurn
import dev.faktor.backend.NativeClient
import dev.faktor.backend.NativeLiveOutputStream
import dev.faktor.backend.StdoutSink
import dev.faktor.shared.NativeMessage
import java.io.OutputStream
import java.net.InetSocketAddress
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

object NativeLiveStreamSmoke {
    private const val INSTANCE_A = "aaaaaaaaaaaaaaaa"
    private const val INSTANCE_B = "bbbbbbbbbbbbbbbb"
    private const val FOREIGN = "ffffffffffffffff"

    @JvmStatic
    fun main(args: Array<String>) {
        System.setProperty("java.awt.headless", "true")
        runAll()
        println("NATIVE LIVE STREAM SMOKE PASS")
    }

    fun runAll() {
        reducerStateMatrix()
        utf8ByteOffsetsNeverMixWithUtf16()
        liveFramesAdvanceBeforeDurableFlush()
        duplicateOmitReplayResyncIsByteIdentical()
        foreignInstanceFramesAreDropped()
        durableWinsDivergenceAndKeepsNewerLiveSuffix()
        reconnectReconcilesAuthoritativeSnapshot()
        stopTargetsAcceptedOperation()
        queuedFollowUpRendersQueued()
        orderedToolRowSitsBetweenTextRows()
        assistantMarkdownStaysPlainStructuralText()
        streamClientIsSnapshotFirstAndResyncs()
        streamClientNonRetryableRefusalIsUnavailable()
        subscribeBeforeSubmitOpensTheLiveHandshakeFirst()
        println("  live: reducer matrix, UTF-8 offsets, overlay, resync, foreign, Stop, queued, tool order OK")
    }

    // -------------------------------------------------------------- reducer

    fun reducerStateMatrix() {
        var state = TurnLifecycle.empty()
        check(turnDisplayPhase(state) == TurnPhase.IDLE)
        state = turnReducer(state, TurnEvent.Submit(TurnSource.ASK))
        check(turnDisplayPhase(state) == TurnPhase.SUBMITTING)
        state = turnReducer(state, TurnEvent.Receipt("op-1", null, queued = true))
        check(turnDisplayPhase(state) == TurnPhase.QUEUED)
        state = turnReducer(state, TurnEvent.Admitted("op-1"))
        check(turnDisplayPhase(state) == TurnPhase.STARTING)
        state = turnReducer(state, TurnEvent.Machine("waiting_for_model", "op-1"))
        check(turnDisplayPhase(state) == TurnPhase.STREAMING)
        state = turnReducer(state, TurnEvent.Journal("tool_started", "op-1"))
        check(turnDisplayPhase(state) == TurnPhase.TOOL_RUNNING)
        state = turnReducer(state, TurnEvent.Journal("phase_changed", "op-1", "waiting_for_permission"))
        check(turnDisplayPhase(state) == TurnPhase.AWAITING_USER)
        // A late machine tag can never resurrect a terminal turn.
        state = turnReducer(state, TurnEvent.Journal("turn_completed", "op-1"))
        check(turnDisplayPhase(state) == TurnPhase.COMPLETED)
        state = turnReducer(state, TurnEvent.Machine("streaming", "op-1"))
        check(turnDisplayPhase(state) == TurnPhase.COMPLETED, "completed is sticky")

        // A receipt is admission only: it never completes a turn.
        val admittedOnly = turnReducer(
            turnReducer(TurnLifecycle.empty(), TurnEvent.Submit(TurnSource.ASK)),
            TurnEvent.Receipt("op-2", null, queued = false)
        )
        check(!isTerminalPhase(turnDisplayPhase(admittedOnly)), "2xx is not completion")
        check(turnLifecycleIsLive(admittedOnly))

        // turnStatus is durable authority: cancelled/failed are terminal.
        val cancelled = turnReducer(admittedOnly, TurnEvent.TurnStatus("op-2", "cancelled"))
        check(turnDisplayPhase(cancelled) == TurnPhase.INTERRUPTED)
        val failed = turnReducer(admittedOnly, TurnEvent.TurnStatus("op-2", "failed"))
        check(turnDisplayPhase(failed) == TurnPhase.FAILED)
        check(turnPhaseLabel(TurnPhase.STREAMING) == "Generating…")
        check(turnPhaseLabel(TurnPhase.IDLE) == null)

        // Bounded: the newest 4 submissions survive, oldest terminals first.
        var bounded = TurnLifecycle.empty()
        for (index in 1..6) {
            bounded = turnReducer(bounded, TurnEvent.Submit(TurnSource.ASK))
            bounded = turnReducer(bounded, TurnEvent.Receipt("op-$index", null, queued = false))
            bounded = turnReducer(bounded, TurnEvent.TurnStatus("op-$index", "completed"))
        }
        check(bounded.entries.size == TurnLifecycle.MAX_TURNS, "bounded turn entries")
        check(bounded.entries.last().opId == "op-6")
        println("  reducer: idle→submitting→queued→starting→streaming→tool→awaiting→completed/failed OK")
    }

    // ------------------------------------------------------------ UTF-8 bytes

    fun utf8ByteOffsetsNeverMixWithUtf16() {
        // "hé" is 3 UTF-8 bytes but 2 UTF-16 units; the next frame starts at
        // byte 3, not char index 2.
        val wrong = TranscriptModel()
        check(wrong.applyLiveChunk(frame(42, "text", 1, 1, 0, "hé")))
        check(!wrong.applyLiveChunk(frame(42, "text", 2, 2, 2, "llo")), "UTF-16 offset is a gap")
        check(wrong.liveGapSignals == 1L)

        val model = TranscriptModel()
        check(model.applyLiveChunk(frame(42, "text", 1, 1, 0, "hé")))
        check(model.applyLiveChunk(frame(42, "text", 2, 2, 3, "llo")))
        // The emoji is ONE code point / 4 UTF-8 bytes / 2 UTF-16 units.
        check(model.applyLiveChunk(frame(42, "text", 3, 3, 6, "🙂")))
        val grown = model.livePartsForTest()["42:text"]!!
        check(grown.text == "héllo🙂", "whole code points appended: ${grown.text}")
        check(grown.bytes == 10L, "byte counter is exact: ${grown.bytes}")
        println("  utf8: héllo🙂 = ${grown.bytes} bytes (no UTF-16 index mixed)")
    }

    // ------------------------------------------------------- live before durable

    fun liveFramesAdvanceBeforeDurableFlush() {
        withLivePanel { panel, _ ->
            // NO durable message page was ever read: the first token must
            // still be visible (the exact defect this client fixes).
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 0, "First token"))
            )
            panel.flushLiveForTest()
            val text = panel.transcriptTextForTest()
            check(text.contains("Faktor: First token"), "live text before durable flush: $text")
        }
    }

    // ------------------------------------------- duplicate/omit/replay → resync

    fun duplicateOmitReplayResyncIsByteIdentical() {
        withLivePanel { panel, daemon ->
            daemon.snapshotJson = snapshotJson(
                sequence = 60,
                turn = LiveTurn("op-1", "active"),
                parts = listOf(
                    part(42, "text", revision = 5, text = "Hello world from")
                )
            )
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 0, "Hello"))
            )
            panel.flushLiveForTest()
            // Duplicate of the SAME frame: dropped before any render.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 0, "Hello"))
            )
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 2, 2, 5, " wo"))
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: Hello wo"))

            // A reordered stale frame (old revision): dropped.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 3, 1, 0, "Hello"))
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: Hello wo"))

            // OMITTED range: revision jumps 2 → 5 and the offset jumps.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 6, 5, 100, "lost"))
            )
            panel.flushLiveForTest()
            check(panel.liveGapSignalsForTest() >= 1, "an offset/revision gap requests resync")
            check(panel.transcriptTextForTest().contains("Faktor: Hello wo"))
            check(!panel.transcriptTextForTest().contains("lost"), "a gap never fabricates text")

            // The authoritative snapshot (fetched by the panel resync) lands.
            await("resync snapshot reconciles") {
                panel.transcriptTextForTest().contains("Faktor: Hello world from")
            }
            check(!panel.transcriptTextForTest().contains("Hello woHello"), "no duplicate text")

            // Streaming continues EXACTLY at the snapshot byte offset.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 7, 6, 16, " the server"))
            )
            panel.flushLiveForTest()
            check(
                panel.transcriptTextForTest().contains("Faktor: Hello world from the server"),
                panel.transcriptTextForTest()
            )
            check(panel.liveDuplicatesDroppedForTest() >= 2, "duplicate/replay frames were dropped")
        }
    }

    // --------------------------------------------------------- foreign instance

    fun foreignInstanceFramesAreDropped() {
        withLivePanel { panel, _ ->
            panel.onLiveMessageForTest(
                LiveOutputMessage.Snapshot(
                    snapshot(INSTANCE_A, listOf(part(42, "text", 0, "base")))
                )
            )
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 900, 50, 500, "from-an-old-daemon", FOREIGN))
            )
            panel.flushLiveForTest()
            check(panel.liveForeignDroppedForTest() == 1L, "foreign-instance frame dropped")
            check(panel.transcriptTextForTest().contains("Faktor: base"))
            check(!panel.transcriptTextForTest().contains("from-an-old-daemon"))

            // The restarted daemon is adopted through ITS snapshot.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Snapshot(snapshot(INSTANCE_B, listOf(part(42, "text", 0, "base"))))
            )
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 4, " after-restart", INSTANCE_B))
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: base after-restart"))
        }
    }

    // ----------------------------------------------- durable vs live suffix

    fun durableWinsDivergenceAndKeepsNewerLiveSuffix() {
        val model = TranscriptModel()
        model.upsertDurable(listOf(message(2, 42, "assistant", "Hello")), { false })
        check(model.applyLiveChunk(frame(42, "text", 1, 1, 5, " world")))
        check(model.rowsWithLive().single().text == "Hello world")

        // A STALE saved snapshot that is only a prefix never overwrites the
        // newer live suffix.
        model.applyLiveSnapshot(
            snapshot(INSTANCE_A, listOf(part(42, "text", 0, "Hello")))
        )
        check(model.rowsWithLive().single().text == "Hello world", "stale prefix kept live suffix")

        // A diverged durable page wins byte-for-byte (no duplicate).
        model.upsertDurable(listOf(message(2, 42, "assistant", "Hello there")), { false })
        check(model.rowsWithLive().single().text == "Hello there", "durable authority wins")
        println("  overlay: durable wins divergence, live suffix wins a stale prefix")
    }

    // -------------------------------------------------------- reconnect

    fun reconnectReconcilesAuthoritativeSnapshot() {
        withLivePanel { panel, _ ->
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 0, "Hello"))
            )
            panel.flushLiveForTest()
            // Reconnect handshake: the snapshot is at least as new → adopt.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Snapshot(
                    LiveSnapshot(
                        INSTANCE_A, 7, 60, 12, LiveTurn("op-1", "active"),
                        listOf(part(42, "text", 5, "Hello world"))
                    )
                )
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: Hello world"))
            check(!panel.transcriptTextForTest().contains("HelloHello"))
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 61, 6, 11, "!"))
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: Hello world!"))
        }
    }

    // ------------------------------------------------------------- Stop target

    fun stopTargetsAcceptedOperation() {
        withLivePanel { panel, daemon ->
            // Partial text already streamed before Stop must survive it.
            panel.onLiveMessageForTest(
                LiveOutputMessage.Chunk(frame(42, "text", 1, 1, 0, "partial reply"))
            )
            panel.flushLiveForTest()
            check(panel.transcriptTextForTest().contains("Faktor: partial reply"))
            panel.dispatchTurnForTest(TurnEvent.Submit(TurnSource.ASK))
            panel.dispatchTurnForTest(TurnEvent.Receipt("op-1", null, queued = false))
            panel.dispatchTurnForTest(TurnEvent.Machine("streaming", "op-1"))
            check(panel.turnStopOpForTest() == "op-1")
            panel.triggerAbortForTest()
            await("the abort request carries the accepted op id") {
                daemon.abortBodies.any { it.contains("\"op_id\":\"op-1\"") }
            }
            await("the typed abort ack marks the turn interrupted") {
                panel.turnPhaseForTest() == "interrupted"
            }
            check(panel.workStateChipTextForTest() == "Stopped")
            check(
                panel.transcriptTextForTest().contains("Faktor: partial reply"),
                "partial text is kept after Stop, never silently deleted"
            )
            check(!daemon.abortBodies.any { it.contains("op-2") })
        }
    }

    // ------------------------------------------------------------- queued row

    fun queuedFollowUpRendersQueued() {
        withLivePanel { panel, _ ->
            panel.dispatchTurnForTest(TurnEvent.Submit(TurnSource.ASK))
            panel.dispatchTurnForTest(TurnEvent.Receipt("op-1", null, queued = false))
            panel.dispatchTurnForTest(TurnEvent.Machine("streaming", "op-1"))
            // A second logical submission is admitted while the first streams.
            panel.dispatchTurnForTest(TurnEvent.Submit(TurnSource.ASK))
            panel.dispatchTurnForTest(TurnEvent.Receipt("op-2", null, queued = true))
            check(panel.turnPhaseForTest() == "queued")
            check(panel.workStateChipTextForTest() == "Queued")
            // Stop still targets the RUNNING turn ahead of the queued one.
            check(panel.turnStopOpForTest() == "op-1")
        }
    }

    // ----------------------------------------------------- ordered tool parts

    fun orderedToolRowSitsBetweenTextRows() {
        val view = TranscriptView()
        val sink = TranscriptSink(view, null, UiWorkMetrics(), 100_000, 2_000)
        val model = TranscriptModel()
        applyModelOps(model.upsertDurable(listOf(message(1, 1, "user", "go")), { false }), sink)
        applyModelOps(model.upsertDurable(listOf(message(2, 2, "assistant", "")), { false }), sink)
        check(model.applyLiveChunk(frame(2, "text", 1, 1, 0, "streamed")))
        check(model.applyLiveChunk(frame(2, "tool", 2, 1, 0, "run-tests")))
        applyModelOps(model.liveOps(), sink)
        sink.flush()
        // A later durable message must land AFTER the tool row (the tool sits
        // between the two text rows, in stream order).
        applyModelOps(
            model.upsertDurable(listOf(message(3, 3, "assistant", "next"))) { sink.locatedRow(it.render()) },
            sink
        )
        sink.flush()
        val text = view.plainText()
        val toolAt = text.indexOf("[tool]")
        val nextAt = text.indexOf("next")
        check(toolAt >= 0, "tool row rendered: $text")
        check(nextAt > toolAt, "tool row sits between text rows: $text")
    }

    // -------------------------------------------------------------- markdown

    fun assistantMarkdownStaysPlainStructuralText() {
        // The rich parser: structure is first-class, hostile text is only
        // ever characters, and unsafe link schemes can never become links.
        val blocks = RichMarkdown.parse(
            "# Title\n- one\n> quote\n```kotlin\nval x = 1\n```\n**bold** and `code`"
        )
        val kinds = blocks.map { it::class.java.simpleName }
        check(
            kinds == listOf("Heading", "BulletList", "Quote", "Code", "Paragraph"),
            "block sequence: $kinds"
        )
        val paragraph = blocks.last() as MarkdownBlock.Paragraph
        check(paragraph.plainText() == "bold and code", paragraph.plainText())
        check(
            paragraph.spans.any { it is InlineSpan.Strong } &&
                paragraph.spans.any { it is InlineSpan.CodeSpan },
            "inline styles survive the parse: ${paragraph.spans}"
        )
        val code = blocks[3] as MarkdownBlock.Code
        check(code.language == "kotlin" && code.code == "val x = 1", "$code")
        val allText = blocks.joinToString("\n") { it.plainText() }
        check(!allText.contains("<html"), "no HTML is ever produced: $allText")

        // Unsafe schemes NEVER become links (nothing clickable, URL visible).
        val hostile = RichMarkdown.parse(
            "[run](javascript:alert(1)) [x](ide://command) [f](file:///etc/passwd)"
        )
        val links = hostile.flatMap { block ->
            when (block) {
                is MarkdownBlock.Paragraph -> block.spans.filterIsInstance<InlineSpan.Link>()
                else -> emptyList()
            }
        }
        check(links.isEmpty(), "unsafe schemes must never become links: $links")
        check(RichMarkdown.safeUrl("https://example.com") != null)
        check(RichMarkdown.safeUrl("mailto:dev@example.com") != null)
        check(RichMarkdown.safeUrl("javascript:alert(1)") == null)
        check(RichMarkdown.safeUrl("file:///etc/passwd") == null)
        check(RichMarkdown.safeUrl("ide://run-command") == null)
        check(RichMarkdown.safeUrl("data:text/html,<script>") == null)
    }

    // --------------------------------------------------------- live client

    fun streamClientIsSnapshotFirstAndResyncs() {
        val server = LiveFakeServer()
        server.outputFrames = buildString {
            append(sse("snapshot", snapshotJson(sequence = 0, turn = null, parts = emptyList())))
            append(sse("live", chunkJson(42, "text", 1, 1, 0, "Hello")))
            // Duplicate/replay of sequence 1 (never redelivered).
            append(sse("live", chunkJson(42, "text", 1, 1, 0, "Hello")))
            append(sse("live", chunkJson(42, "text", 2, 2, 5, " wo")))
            // Foreign instance lineage (never delivered).
            append(sse("live", chunkJson(42, "text", 3, 3, 0, "FOREIGN", FOREIGN)))
            append(sse("turn", "{\"instance\":\"$INSTANCE_A\",\"session\":7,\"turn\":\"op-1\",\"status\":\"active\"}"))
            append(sse("resync", "{\"instance\":\"$INSTANCE_A\",\"session\":7,\"reason\":\"lagged\",\"dropped\":2}"))
            append(sse("live", chunkJson(42, "text", 61, 6, 15, " from-server")))
        }
        server.snapshotJson = snapshotJson(
            sequence = 60,
            turn = LiveTurn("op-1", "active"),
            parts = listOf(part(42, "text", revision = 5, text = "Hello world fro"))
        )
        server.start()
        val messages = Collections.synchronizedList(ArrayList<LiveOutputMessage>())
        val errors = Collections.synchronizedList(ArrayList<String>())
        val stream = NativeLiveOutputStream(
            server.baseUrl, "tok", "7",
            snapshotFetcher = { NativeClient(server.baseUrl, "tok").liveSnapshot("7") },
            minBackoffMs = 10,
            maxBackoffMs = 20,
            onMessage = { messages.add(it) },
            onError = { errors.add(it.message ?: "error") }
        )
        try {
            stream.start()
            await("the client delivers the post-resync chunk") {
                messages.any { it is LiveOutputMessage.Chunk && it.frame.content == " from-server" }
            }
            val chunks = messages.filterIsInstance<LiveOutputMessage.Chunk>().map { it.frame }
            check(chunks.first().content == "Hello", "first delivered chunk")
            check(chunks.none { it.content == "FOREIGN" }, "foreign instance never delivered")
            check(chunks.count { it.content == "Hello" } == 1, "duplicate/replay dropped")
            check(chunks.any { it.content == " from-server" }, "post-resync streaming continues")
            check(messages.any { it is LiveOutputMessage.Snapshot && it.snapshot.sequence == 60L })
            check(messages.any { it is LiveOutputMessage.Turn && it.frame.turn == "op-1" })
            check(server.outputRequests.get() >= 1)
            check(server.snapshotRequests.get() >= 1, "resync fetched the authoritative snapshot")
            check(errors.any { it.contains("foreign") }, errors.toString())
            check(stream.cursor == 61L, "cursor follows the delivered sequences: ${stream.cursor}")
        } finally {
            stream.stop()
            server.stop()
        }
    }

    fun streamClientNonRetryableRefusalIsUnavailable() {
        val server = LiveFakeServer()
        server.outputStatus = 404
        server.start()
        val statuses = Collections.synchronizedList(ArrayList<String>())
        val stream = NativeLiveOutputStream(
            server.baseUrl, "tok", "7",
            minBackoffMs = 10,
            maxBackoffMs = 20,
            onMessage = {},
            onStatus = { status, _ -> statuses.add(status) },
            onError = {}
        )
        try {
            stream.start()
            await("a non-retryable route refusal ends in unavailable") {
                stream.status == "unavailable"
            }
            check(server.outputRequests.get() == 1, "a terminal refusal is never retried")
            check(statuses.contains("unavailable"))
        } finally {
            stream.stop()
            server.stop()
        }
        println("  client: snapshot-first, replay/foreign dropped, resync fetched, 404 unavailable")
    }

    // --------------------------------------------------- subscribe-before-submit

    fun subscribeBeforeSubmitOpensTheLiveHandshakeFirst() {
        val server = LiveFakeServer()
        server.outputFrames = sse(
            "snapshot",
            snapshotJson(sequence = 0, turn = null, parts = emptyList())
        )
        server.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            server.server.address.port, "live-test", process, StdoutSink(process)
        )
        val service = FaktorFrontendService(Paths.get("unused"), tempDir("live-order-"))
        val panel = FaktorChatPanel(service)
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            service.useSession("7")
            panel.setTaskFieldsForTest("stream this", "")
            panel.submitAskForTest()
            await("the prompt is accepted") { server.promptRequests.get() >= 1 }
            val order = ArrayList(server.requestOrder)
            val liveAt = order.indexOf("live")
            val promptAt = order.indexOf("prompt")
            check(liveAt >= 0, "the live subscription was opened: $order")
            check(promptAt >= 0, "the prompt was posted: $order")
            check(liveAt < promptAt, "the live handshake is established BEFORE the prompt POST: $order")
        } finally {
            panel.shutdown()
            service.stop()
            server.stop()
            process.destroyForcibly()
        }
    }

    // ------------------------------------------------------------- helpers

    private fun frame(
        messageId: Long,
        kind: String,
        sequence: Long,
        revision: Long,
        offset: Long,
        content: String,
        instance: String = INSTANCE_A
    ): LiveChunkFrame = LiveChunkFrame(
        instance = instance,
        session = 7,
        messageId = messageId,
        kind = kind,
        partId = "$messageId:$kind",
        turn = "op-1",
        attempt = 0,
        sequence = sequence,
        revision = revision,
        offset = offset,
        content = content
    )

    private fun part(
        messageId: Long,
        kind: String,
        revision: Long,
        text: String
    ): LiveSnapshotPart {
        val bytes = text.toByteArray(Charsets.UTF_8).size.toLong()
        return LiveSnapshotPart(
            partId = "$messageId:$kind",
            messageId = messageId,
            kind = kind,
            turn = "op-1",
            attempt = 0,
            revision = revision,
            offset = bytes,
            text = text,
            durableBytes = bytes,
            durableRevision = revision,
            liveTail = true
        )
    }

    private fun snapshot(
        instance: String,
        parts: List<LiveSnapshotPart>,
        sequence: Long = 0,
        turn: LiveTurn? = null
    ): LiveSnapshot = LiveSnapshot(instance, 7, sequence, 0, turn, parts)

    private fun snapshotJson(
        sequence: Long,
        turn: LiveTurn?,
        parts: List<LiveSnapshotPart>
    ): String {
        val partsJson = parts.joinToString(",") { part ->
            "{\"partId\":\"" + part.partId + "\",\"messageId\":" + part.messageId +
                ",\"kind\":\"" + part.kind + "\",\"turn\":\"" + (part.turn ?: "") +
                "\",\"attempt\":" + part.attempt + ",\"revision\":" + part.revision +
                ",\"offset\":" + part.offset + ",\"text\":\"" + part.text +
                "\",\"durableBytes\":" + part.durableBytes +
                ",\"durableRevision\":" + part.durableRevision +
                ",\"liveTail\":" + part.liveTail + "}"
        }
        val turnJson = if (turn == null) "null" else
            "{\"op\":\"" + turn.op + "\",\"status\":\"" + turn.status + "\"}"
        return "{\"instance\":\"$INSTANCE_A\",\"session\":7,\"sequence\":$sequence," +
            "\"watermark\":3,\"turn\":$turnJson,\"parts\":[$partsJson]}"
    }

    private fun chunkJson(
        messageId: Long,
        kind: String,
        sequence: Long,
        revision: Long,
        offset: Long,
        content: String,
        instance: String = INSTANCE_A
    ): String = "{\"instance\":\"$instance\",\"session\":7,\"messageId\":$messageId," +
        "\"kind\":\"$kind\",\"partId\":\"$messageId:$kind\",\"turn\":\"op-1\"," +
        "\"attempt\":0,\"sequence\":$sequence,\"revision\":$revision," +
        "\"offset\":$offset,\"content\":\"$content\"}"

    private fun sse(event: String, data: String): String =
        "event: $event\ndata: $data\n\n"

    private fun message(seq: Long, id: Long, role: String, text: String): NativeMessage =
        NativeMessage(seq = seq, id = id, role = role, createdMs = 0L, text = text)

    private fun applyModelOps(ops: List<TranscriptRowOp>, sink: TranscriptSink) {
        for (op in ops) {
            when (op) {
                is TranscriptRowOp.Upsert -> sink.upsertRow(
                    op.key.render(), op.payload, op.beforeKey?.render()
                )
                is TranscriptRowOp.Rekey -> sink.rekeyRow(
                    op.key.render(), op.newKey.render(), op.payload, op.beforeKey?.render()
                )
            }
        }
    }

    /**
     * The real panel + service against a canned daemon (external fixture):
     * the live listener path, the reducer and the Stop/abort wire are the
     * production ones.
     */
    private fun withLivePanel(block: (FaktorChatPanel, LiveCapture) -> Unit) {
        val daemon = TranscriptFakeDaemon()
        val capture = LiveCapture()
        daemon.on("GET", "/native/health") { _, response ->
            response.json(200, "{\"ok\":true,\"version\":\"0.0.0-live-test\"}")
        }
        daemon.on("GET", "/native/ready") { _, response ->
            response.json(200, "{\"ready\":true}")
        }
        daemon.on("POST", "/native/session/7/abort") { request, response ->
            capture.abortBodies.add(request.body)
            response.json(200, "{\"aborted\":[\"op-1\"]}")
        }
        daemon.on("GET", "/native/session/7/live/snapshot") { _, response ->
            response.json(200, capture.snapshotJson)
        }
        daemon.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            daemon.server.localPort, "live-test", process, StdoutSink(process)
        )
        val service = FaktorFrontendService(Paths.get("unused"), tempDir("live-"))
        val panel = FaktorChatPanel(service)
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            service.useSession("7")
            block(panel, capture)
        } finally {
            panel.shutdown()
            service.stop()
            daemon.stop()
            process.destroyForcibly()
        }
    }

    internal class LiveCapture {
        val abortBodies: MutableList<String> = Collections.synchronizedList(ArrayList())
        @Volatile
        var snapshotJson: String = "{\"instance\":\"$INSTANCE_A\",\"session\":7," +
            "\"sequence\":60,\"watermark\":3,\"turn\":null,\"parts\":[]}"
    }

    private fun tempDir(prefix: String): Path =
        Files.createTempDirectory("faktor-$prefix").also { it.toFile().deleteOnExit() }

    private fun javaBinary(): String = Paths.get(
        System.getProperty("java.home"), "bin", "java"
    ).toString()

    private fun await(what: String, timeoutMs: Long = 10_000L, condition: () -> Boolean) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return
            Thread.sleep(20L)
        }
        check(condition()) { "timed out waiting for: $what" }
    }
}

/**
 * A fake daemon serving the live-output SSE route + the authoritative
 * snapshot route (the external fixture for the client contract).
 */
private class LiveFakeServer {
    val server = com.sun.net.httpserver.HttpServer.create(
        InetSocketAddress("127.0.0.1", 0), 0
    )
    @Volatile
    var outputFrames: String = ""
    @Volatile
    var snapshotJson: String = "{}"
    @Volatile
    var outputStatus: Int = 200
    val outputRequests = AtomicInteger()
    val snapshotRequests = AtomicInteger()
    val promptRequests = AtomicInteger()
    val requestOrder: MutableList<String> = Collections.synchronizedList(ArrayList())
    private val release = CountDownLatch(1)

    val baseUrl: String
        get() = "http://127.0.0.1:" + server.address.port

    fun start() {
        server.createContext("/") { exchange ->
            try {
                val path = exchange.requestURI.path
                if (path == "/native/health") {
                    json(exchange, 200, "{\"ok\":true,\"version\":\"0.0.0-live-order\"}")
                } else if (path == "/native/ready") {
                    json(exchange, 200, "{\"ready\":true}")
                } else if (path.endsWith("/projection")) {
                    json(
                        exchange, 200,
                        "{\"session\":{\"id\":\"7\",\"title\":\"t\",\"provider\":\"p\"," +
                            "\"model\":\"m\",\"lifecycle\":\"open\"}," +
                            "\"state\":{\"machine\":\"idle\",\"label\":\"idle\",\"active\":false," +
                            "\"terminal\":false},\"activeModel\":null,\"activeTool\":null," +
                            "\"progress\":null,\"filesChanged\":[],\"lastCheckpoint\":null," +
                            "\"verification\":[],\"contextUsage\":null,\"queued\":0}"
                    )
                } else if (path == "/native/messages") {
                    json(exchange, 200, "{\"messages\":[],\"hasMore\":false,\"nextBefore\":null}")
                } else if (path.endsWith("/prompt")) {
                    requestOrder.add("prompt")
                    promptRequests.incrementAndGet()
                    json(exchange, 200, "{\"op_id\":\"op-live\",\"accepted\":true,\"queued\":false}")
                } else if (path.endsWith("/live/output")) {
                    requestOrder.add("live")
                    outputRequests.incrementAndGet()
                    if (outputStatus != 200) {
                        val bytes = "{\"error\":{\"code\":\"not_found\",\"message\":\"no live\",\"retryable\":false}}"
                            .toByteArray(Charsets.UTF_8)
                        exchange.sendResponseHeaders(outputStatus, bytes.size.toLong())
                        exchange.responseBody.use { it.write(bytes) }
                        return@createContext
                    }
                    exchange.responseHeaders.add("Content-Type", "text/event-stream")
                    exchange.responseHeaders.add("Cache-Control", "no-cache")
                    exchange.sendResponseHeaders(200, 0)
                    exchange.responseBody.use { out ->
                        write(out, outputFrames)
                        release.await(15, TimeUnit.SECONDS)
                    }
                } else if (path.endsWith("/live/snapshot")) {
                    snapshotRequests.incrementAndGet()
                    val bytes = snapshotJson.toByteArray(Charsets.UTF_8)
                    exchange.responseHeaders.add("Content-Type", "application/json")
                    exchange.sendResponseHeaders(200, bytes.size.toLong())
                    exchange.responseBody.use { it.write(bytes) }
                } else {
                    val bytes = "{\"error\":{\"code\":\"not_found\",\"message\":\"no route\",\"retryable\":false}}"
                        .toByteArray(Charsets.UTF_8)
                    exchange.sendResponseHeaders(404, bytes.size.toLong())
                    exchange.responseBody.use { it.write(bytes) }
                }
            } catch (e: Exception) {
                // The client hung up; the assertions already observed it.
            } finally {
                runCatching { exchange.close() }
            }
        }
        server.executor = Executors.newCachedThreadPool { runnable ->
            Thread(runnable, "live-fake").apply { isDaemon = true }
        }
        server.start()
    }

    fun stop() {
        release.countDown()
        server.stop(0)
    }

    private fun json(exchange: com.sun.net.httpserver.HttpExchange, status: Int, body: String) {
        val bytes = body.toByteArray(Charsets.UTF_8)
        exchange.responseHeaders.add("Content-Type", "application/json")
        exchange.sendResponseHeaders(status, bytes.size.toLong())
        exchange.responseBody.use { it.write(bytes) }
    }

    private fun write(out: OutputStream, text: String) {
        out.write(text.toByteArray(Charsets.UTF_8))
        out.flush()
    }
}

/** Message-carrying check (the stdlib overload only takes a lazy message). */
private fun check(value: Boolean, message: String) {
    if (!value) throw AssertionError(message)
}
