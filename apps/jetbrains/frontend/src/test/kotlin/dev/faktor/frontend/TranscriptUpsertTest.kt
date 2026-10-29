// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
// Adversarial transcript-upsert tests for the P0 "permanently incomplete
// replies" fix (audit item 1) and the stream-milestone copy fix (audit item
// 3). The ORACLE is the audit contract itself: durable message pages are
// EXTERNAL fixtures served by a canned fake daemon (the SUT never grades
// itself), and every case tries to break the renderer:
//
//  - an EMPTY assistant row followed by THREE content updates to the SAME
//    seq: the final transcript carries the full content exactly once, in
//    order (the pre-fix `seq > renderedSeq` filter froze the row forever);
//  - an optimistic (provisional) user row followed by its durable twin:
//    exactly ONE row survives, reconciled by identity;
//  - updates that ARRIVE OUT OF ORDER (a lower-seq row after higher ones)
//    still converge to seq order, and a stale shorter snapshot of an
//    already-grown message is refused by the revision guard;
//  - a DOWNWARD content revision (impossible in production — durable text
//    is append-only) is refused by the pure model's revision guard;
//  - raw `[faktor] [event] id=...` milestone lines are gone: the transcript
//    carries human copy while the wire kind + journal id land in the
//    Diagnostics details surface.
//
// Isolation: every case uses its own temp data dir + workspace and its own
// fake-daemon socket; nothing reads or writes the developer's ~/.faktor.
package dev.faktor.frontend

import dev.faktor.backend.BackendConnection
import dev.faktor.backend.StdoutSink
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeMessage
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.util.Collections
import java.util.concurrent.atomic.AtomicReference
import org.junit.Test

class TranscriptUpsertTest {

    // -------------------------------------------------- empty row + updates

    @Test
    fun emptyAssistantRowThenThreeContentUpdatesConvergesExactlyOnce() {
        val page = AtomicReference<List<NativeMessage>>(listOf(message(1L, 1L, "assistant", "")))
        withTranscriptHost(page) { panel, _ ->
            // 1. First observation: the assistant row is EMPTY — nothing
            // renders, but the model reserves the row's ordered position.
            panel.refreshMessagesForTest()
            assertEquals("", panel.transcriptTextForTest(), "an empty row renders nothing")

            // 2..4. THREE content updates to the SAME seq/id.
            page.set(listOf(message(1L, 1L, "assistant", "Part one.")))
            panel.refreshMessagesForTest()
            page.set(listOf(message(1L, 1L, "assistant", "Part one. Part two.")))
            panel.refreshMessagesForTest()
            page.set(listOf(message(1L, 1L, "assistant", "Part one. Part two. Part three.")))
            panel.refreshMessagesForTest()

            val text = panel.transcriptTextForTest()
            assertEquals(
                "assistant: Part one. Part two. Part three.\n",
                text,
                "the final row must carry the full content exactly once"
            )
            assertEquals(1, countOccurrences(text, "assistant: "), "exactly one assistant row")
            assertEquals(0L, panel.transcriptRevisionGuardHitsForTest(), "growth is never guarded")
        }
    }

    // ------------------------------------- provisional row + durable twin

    @Test
    fun provisionalUserRowReconcilesWithItsDurableTwinIntoOneRow() {
        val page = AtomicReference<List<NativeMessage>>(emptyList())
        withTranscriptHost(page) { panel, daemon ->
            daemon.on("POST", "/native/session/7/prompt") { _, response ->
                response.json(
                    200,
                    "{\"op_id\":\"op-1\",\"accepted\":true,\"queued\":false}"
                )
            }
            // The durable twin arrives with the SAME text the optimistic row
            // rendered (the daemon persisted the prompt).
            page.set(listOf(message(7L, 1L, "user", "hello durable twin")))
            panel.setTaskFieldsForTest("hello durable twin", "")
            panel.submitAskForTest()
            await("the durable twin reconciles into ONE row") {
                val text = panel.transcriptTextForTest()
                text.contains("user: hello durable twin") &&
                    !text.contains("you: hello durable twin")
            }
            val text = panel.transcriptTextForTest()
            assertEquals(
                1,
                countOccurrences(text, "hello durable twin"),
                "the optimistic row and its durable twin must be ONE row: $text"
            )
            assertEquals(0L, panel.transcriptRevisionGuardHitsForTest())
        }
    }

    // --------------------------------------- out-of-order + stale snapshot

    @Test
    fun outOfOrderUpdatesConvergeAndStaleSnapshotsAreRefused() {
        val page = AtomicReference<List<NativeMessage>>(listOf(message(5L, 5L, "assistant", "second")))
        withTranscriptHost(page) { panel, _ ->
            panel.refreshMessagesForTest()

            // A LOWER-seq row arrives after the higher one: it must insert
            // BEFORE it (seq decides ordering, both directions).
            page.set(
                listOf(
                    message(5L, 5L, "assistant", "second"),
                    message(4L, 4L, "assistant", "first")
                )
            )
            panel.refreshMessagesForTest()
            var text = panel.transcriptTextForTest()
            assertTrue(
                text.indexOf("assistant: first") < text.indexOf("assistant: second"),
                "the lower-seq row must render first, got: $text"
            )

            // A content update to seq 5 lands in place (same seq, new text).
            page.set(
                listOf(
                    message(5L, 5L, "assistant", "second and more"),
                    message(4L, 4L, "assistant", "first")
                )
            )
            panel.refreshMessagesForTest()
            text = panel.transcriptTextForTest()
            assertTrue(text.contains("assistant: second and more"), text)
            assertEquals(1, countOccurrences(text, "assistant: second"), text)

            // A STALE page (seq 5 regressed to the shorter text) arrives
            // late: the revision guard refuses the downgrade.
            page.set(
                listOf(
                    message(5L, 5L, "assistant", "second"),
                    message(4L, 4L, "assistant", "first")
                )
            )
            panel.refreshMessagesForTest()
            text = panel.transcriptTextForTest()
            assertTrue(
                text.contains("assistant: second and more"),
                "a stale shorter snapshot must never regress the row: $text"
            )
            assertEquals(1L, panel.transcriptRevisionGuardHitsForTest())

            // An identical fresh page is a pure no-op.
            page.set(
                listOf(
                    message(5L, 5L, "assistant", "second and more"),
                    message(4L, 4L, "assistant", "first")
                )
            )
            panel.refreshMessagesForTest()
            assertEquals(text, panel.transcriptTextForTest(), "an identical page is a no-op")
        }
    }

    // ------------------------------------------------ downward revision

    @Test
    fun downwardContentRevisionIsRefusedByTheModelGuard() {
        val model = TranscriptModel()
        val full = message(9L, 1L, "assistant", "full durable content")
        model.upsertDurable(listOf(full)) { false }
        assertEquals("full durable content", model.rows().single().text)

        // A SHORTER observation of the same id: impossible in production
        // (durable text only grows), but the guard must prove the rule.
        val ops = model.upsertDurable(listOf(full.copy(text = "full"))) { false }
        assertTrue(ops.isEmpty(), "a downward revision must produce no render op")
        assertEquals(1L, model.revisionGuardHits)
        assertEquals("full durable content", model.rows().single().text)
        assertEquals(contentRevision("full durable content"), model.rows().single().revision)

        // An EQUAL-LENGTH rewrite is a tie: the newest observation wins.
        val tied = model.upsertDurable(listOf(full.copy(text = "FULL DURABLE CONTENT"))) { false }
        assertEquals(1, tied.size, "a tie accepts the newest observation")
        assertEquals("FULL DURABLE CONTENT", model.rows().single().text)
    }

    // --------------------------------------------------- milestone copy

    @Test
    fun streamMilestonesRenderHumanCopyAndKeepIdsInDiagnostics() {
        val service = FaktorFrontendService(Paths.get("unused-binary"), tempDir("milestone-"))
        val panel = FaktorChatPanel(service)
        try {
            service.useSession("7")
            panel.onEvent(
                dev.faktor.backend.NativeSseEvent(42L, "turn_completed", JsonValue.Obj(emptyMap()))
            )
            panel.flushStreamForTest()
            val text = panel.transcriptTextForTest()
            assertTrue(text.contains("Turn completed"), text)
            assertTrue(!text.contains("[turn_completed]"), "no raw wire kind in the transcript: $text")
            assertTrue(!text.contains("id=42"), "no raw journal id in the transcript: $text")
            assertTrue(
                !text.contains("[faktor] ["),
                "the raw milestone format must be gone: $text"
            )
            val milestone = panel.diagnosticsMilestoneForTest()
            assertTrue(milestone.contains("turn_completed"), milestone)
            assertTrue(milestone.contains("42"), "the journal id stays in diagnostics: $milestone")
            assertEquals("Turn completed", milestoneCopy("turn_completed"))
            assertEquals("Reconnecting…", milestoneCopy("replay_started"))
            assertEquals("Streaming paused", milestoneCopy("suspended"))
        } finally {
            panel.shutdown()
            service.stop()
        }
    }

    // ------------------------------------------------------------ harness

    private fun message(seq: Long, id: Long, role: String, text: String): NativeMessage =
        NativeMessage(seq = seq, id = id, role = role, createdMs = 0L, text = text)

    /**
     * The real panel + service against a canned fake daemon: the messages
     * page is the EXTERNAL fixture every scenario mutates; the panel drives
     * the production refresh path ([FaktorChatPanel.refreshMessagesForTest]).
     */
    private fun withTranscriptHost(
        page: AtomicReference<List<NativeMessage>>,
        block: (FaktorChatPanel, TranscriptFakeDaemon) -> Unit
    ) {
        val daemon = TranscriptFakeDaemon()
        daemon.on("GET", "/native/health") { _, response ->
            response.json(200, "{\"ok\":true,\"version\":\"0.0.0-transcript-test\"}")
        }
        daemon.on("GET", "/native/ready") { _, response ->
            response.json(200, "{\"ready\":true}")
        }
        daemon.on("GET", "/session/7/projection") { _, response ->
            response.json(
                200,
                "{\"session\":{\"id\":\"7\",\"title\":\"t\",\"provider\":\"p\",\"model\":\"m\"," +
                    "\"lifecycle\":\"open\"}," +
                    "\"state\":{\"machine\":\"idle\",\"label\":\"idle\",\"active\":false," +
                    "\"terminal\":false}," +
                    "\"activeModel\":null,\"activeTool\":null,\"progress\":null," +
                    "\"filesChanged\":[],\"lastCheckpoint\":null,\"verification\":[]," +
                    "\"contextUsage\":null,\"queued\":0}"
            )
        }
        daemon.on("GET", "/native/messages") { _, response ->
            val rows = page.get().joinToString(",") { entry ->
                "{\"seq\":${entry.seq},\"id\":${entry.id},\"role\":\"${entry.role}\"," +
                    "\"createdMs\":${entry.createdMs},\"parts\":[{\"kind\":\"text\"," +
                    "\"data\":{\"text\":\"${entry.text}\"}}],\"data\":{\"text\":\"${entry.text}\"}}"
            }
            response.json(
                200,
                "{\"messages\":[$rows],\"hasMore\":false,\"nextBefore\":null}"
            )
        }
        daemon.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            daemon.server.localPort, "transcript-test", process, StdoutSink(process)
        )
        val service = FaktorFrontendService(Paths.get("unused"), tempDir("transcript-"))
        val panel = FaktorChatPanel(service)
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            service.useSession("7")
            block(panel, daemon)
        } finally {
            panel.shutdown()
            service.stop()
            daemon.stop()
            process.destroyForcibly()
        }
    }

    private fun tempDir(prefix: String): Path =
        Files.createTempDirectory("faktor-$prefix").also { it.toFile().deleteOnExit() }

    private fun javaBinary(): String = Paths.get(
        System.getProperty("java.home"), "bin", "java"
    ).toString()

    private fun countOccurrences(text: String, needle: String): Int {
        var count = 0
        var index = text.indexOf(needle)
        while (index >= 0) {
            count += 1
            index = text.indexOf(needle, index + needle.length)
        }
        return count
    }

    private fun await(what: String, timeoutMs: Long = 10_000L, condition: () -> Boolean) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return
            Thread.sleep(25L)
        }
        assertTrue(condition(), "timed out waiting for: $what")
    }
}

/**
 * A minimal canned HTTP daemon (the external fixture): fixed routes plus a
 * swappable /native/messages page. Requests are recorded for diagnostics.
 */
internal class TranscriptFakeDaemon {
    val server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
    private val handlers =
        Collections.synchronizedMap(HashMap<String, (TranscriptRequest, TranscriptResponse) -> Unit>())
    @Volatile
    private var running = true

    fun on(method: String, path: String, handler: (TranscriptRequest, TranscriptResponse) -> Unit) {
        handlers["$method $path"] = handler
    }

    fun start() {
        val thread = Thread({ acceptLoop() }, "transcript-fake-accept")
        thread.isDaemon = true
        thread.start()
    }

    fun stop() {
        running = false
        try {
            server.close()
        } catch (e: Exception) {
            // Already closed.
        }
    }

    private fun acceptLoop() {
        while (running) {
            val socket = try {
                server.accept()
            } catch (e: Exception) {
                break
            }
            val thread = Thread({ handle(socket) }, "transcript-fake-conn")
            thread.isDaemon = true
            thread.start()
        }
    }

    private fun handle(socket: java.net.Socket) {
        try {
            socket.use {
                val input = BufferedInputStream(socket.getInputStream())
                val requestLine = readLine(input) ?: return
                val parts = requestLine.split(' ')
                if (parts.size < 2) return
                val method = parts[0]
                val target = parts[1]
                val qIndex = target.indexOf('?')
                val path = if (qIndex < 0) target else target.substring(0, qIndex)
                var length = 0
                while (true) {
                    val header = readLine(input) ?: return
                    if (header.isEmpty()) break
                    if (header.startsWith("Content-Length:", ignoreCase = true)) {
                        length = header.substring(header.indexOf(':') + 1).trim().toIntOrNull() ?: 0
                    }
                }
                val body = ByteArray(length)
                var read = 0
                while (read < length) {
                    val n = input.read(body, read, length - read)
                    if (n < 0) break
                    read += n
                }
                val response = TranscriptResponse(BufferedOutputStream(socket.getOutputStream()))
                val handler = handlers["$method $path"]
                if (handler == null) {
                    response.json(
                        404,
                        "{\"error\":{\"code\":\"not_found\",\"message\":\"no route\"," +
                            "\"retryable\":false}}"
                    )
                } else {
                    handler(TranscriptRequest(method, path, String(body, 0, read, Charsets.UTF_8)), response)
                }
            }
        } catch (e: Exception) {
            // Client disconnect; assertions already observed the response.
        }
    }

    private fun readLine(input: BufferedInputStream): String? {
        val line = StringBuilder()
        while (true) {
            val byte = input.read()
            if (byte < 0) return if (line.isEmpty()) null else line.toString()
            when (byte.toByte()) {
                '\n'.code.toByte() -> return line.toString()
                '\r'.code.toByte() -> {}
                else -> line.append(byte.toChar())
            }
        }
    }
}

internal class TranscriptRequest(
    val method: String,
    val path: String,
    val body: String
)

internal class TranscriptResponse(private val out: BufferedOutputStream) {
    fun json(status: Int, body: String) {
        val bytes = body.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 $status OK\r\n" +
            "Content-Type: application/json\r\n" +
            "Content-Length: ${bytes.size}\r\n" +
            "Connection: close\r\n\r\n"
        out.write(head.toByteArray(Charsets.US_ASCII))
        out.write(bytes)
        out.flush()
    }
}
