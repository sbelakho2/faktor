// SSE client for the daemon's native journal event stream
// (`GET /native/session/{id}/events?after=<cursor>`), the stream the native
// server projects from the durable journal. Semantics:
//
//  - every frame carries `event:` (projection type), `id:` (the journal
//    sequence — the resume cursor) and one JSON `data:` line;
//  - heartbeats (`event: heartbeat` / comment keep-alives) are ignored but
//    still advance the cursor when they carry an id;
//  - on a TRANSIENT disconnect the loop reconnects with bounded exponential
//    backoff and resumes from the last delivered frame id, so a reconnect
//    can neither duplicate nor skip events;
//  - a durable-stream protocol violation (malformed frame, discriminator
//    mismatch, impossible cursor, oversized durable frame, unsupported
//    frame version, or the daemon's terminal `journal_read_failed` frame) is
//    NOT transient: the stream enters the stable `blocked` state, stops
//    auto-reconnecting and never silently skips the offending durable event
//    (the cursor does not advance past it). Recovery is explicit through
//    recoverFrom() — refresh from a snapshot cursor / reconnect after a
//    daemon upgrade / run doctor — or by opening a new stream;
//  - the stream runs on one daemon thread; stop() closes the body, which
//    unblocks the reader (no orphan thread, no orphan socket).
package dev.faktor.backend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeProtocolException
import java.io.IOException
import java.io.InputStream
import java.io.InputStreamReader
import java.net.URI
import java.net.URLEncoder
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import java.time.Duration
import java.util.Random

/** One journal frame: [id] is the resume cursor, [event] the projection type. */
data class NativeSseEvent(val id: Long, val event: String, val data: JsonValue)

/**
 * A STABLE durable-stream protocol block: the journal stream itself is
 * corrupt or incompatible with this client, so reconnecting from the same
 * cursor would replay the same offending sequence forever. This is never a
 * transient network failure — the stream stops auto-reconnecting, stays in
 * the `blocked` status, and the cursor does not advance past the block
 * (nothing is silently skipped). [recovery] names the affordances the UI
 * surfaces; [NativeEventStream.recoverFrom] is the explicit exit.
 */
class ProtocolBlocked(
    val kind: Kind,
    val cursor: Long,
    val detail: String,
    val recovery: List<String> = DEFAULT_RECOVERY
) : Exception("durable stream blocked ($kind) at cursor $cursor: $detail") {

    /** The durable-stream corruption/incompatibility class. */
    enum class Kind {
        /** The frame `data:` is not valid JSON. */
        MALFORMED_FRAME,

        /** The `event:` name disagrees with the `data.event` discriminator. */
        DISCRIMINATOR_MISMATCH,

        /** No id, an unreadable id, or an id behind the resume cursor. */
        IMPOSSIBLE_CURSOR,

        /** The frame declares a schema/version this client cannot read. */
        UNSUPPORTED_VERSION,

        /** A durable frame exceeded the configured byte bound. */
        OVERSIZED_FRAME,

        /** The daemon's terminal error frame: the journal cannot be read. */
        JOURNAL_UNREADABLE
    }

    companion object {
        /** The explicit recovery affordances surfaced with a block. */
        val DEFAULT_RECOVERY: List<String> = listOf(
            "refresh from snapshot",
            "reconnect after daemon upgrade",
            "run doctor"
        )
    }
}

/**
 * One reconnecting SSE client for a single session. Listeners are invoked on
 * the stream thread; UI callers must marshal to their own event loop. A
 * durable protocol violation latches [blocked] (a [ProtocolBlocked]) and the
 * loop stops; explicit recovery is [recoverFrom].
 */
class NativeEventStream(
    private val baseUrl: String,
    private val bearerToken: String,
    private val sessionId: String,
    cursor: Long = 0,
    private val maxFrameBytes: Int = DEFAULT_MAX_FRAME_BYTES,
    private val minBackoffMs: Long = 250L,
    private val maxBackoffMs: Long = 15_000L,
    private val timeoutMs: Long = DEFAULT_TIMEOUT_MS,
    private val onEvent: (NativeSseEvent) -> Unit,
    private val onStatus: (String, String?) -> Unit = { _, _ -> },
    private val onError: (Exception) -> Unit = {},
    private val onBlocked: (ProtocolBlocked) -> Unit = {}
) {
    companion object {
        const val DEFAULT_MAX_FRAME_BYTES = 1 shl 20
        const val DEFAULT_TIMEOUT_MS = 30_000L

        /** The only frame schema this client understands. */
        const val SUPPORTED_FRAME_SCHEMA = "faktor-native-event/v1"

        private const val MAX_BACKOFF_ATTEMPT = 20
        private const val MAX_ERROR_BODY_CHARS = 200

        fun forConnection(
            connection: BackendConnection,
            sessionId: String,
            cursor: Long = 0,
            onEvent: (NativeSseEvent) -> Unit,
            onStatus: (String, String?) -> Unit = { _, _ -> },
            onError: (Exception) -> Unit = {},
            onBlocked: (ProtocolBlocked) -> Unit = {}
        ): NativeEventStream = NativeEventStream(
            connection.baseUrl, connection.password, sessionId, cursor,
            onEvent = onEvent, onStatus = onStatus, onError = onError,
            onBlocked = onBlocked
        )
    }

    @Volatile
    private var cursorValue: Long = if (cursor < 0) 0 else cursor

    @Volatile
    private var stopped = true

    @Volatile
    private var stage: String = "stopped"

    @Volatile
    private var activeBody: InputStream? = null

    @Volatile
    private var blockedValue: ProtocolBlocked? = null

    private var thread: Thread? = null

    private val random = Random()

    val cursor: Long
        get() = cursorValue

    val status: String
        get() = stage

    /** The stable protocol block, or null while healthy/transiently retrying. */
    val blocked: ProtocolBlocked?
        get() = blockedValue

    /** Seed the resume cursor (e.g. from a paged `/native/events` read). */
    fun setCursor(value: Long) {
        if (value >= 0) cursorValue = value
    }

    /**
     * Starts streaming; idempotent while running. A [blocked] stream refuses
     * to restart implicitly: recovery is the explicit [recoverFrom].
     */
    fun start() {
        if (blockedValue != null) return
        if (!stopped) return
        stopped = false
        val t = Thread({ loop() }, "faktor-sse-$sessionId")
        t.isDaemon = true
        thread = t
        t.start()
    }

    /**
     * Clears a [blocked] state and restarts from [cursor] — the recovery
     * affordance behind "refresh from snapshot" and "reconnect after daemon
     * upgrade". Returns false (doing nothing) when the stream is not blocked
     * or [cursor] is negative: recovery is never implicit.
     */
    fun recoverFrom(cursor: Long): Boolean {
        if (blockedValue == null) return false
        if (cursor < 0) return false
        stop()
        blockedValue = null
        cursorValue = cursor
        start()
        return true
    }

    /** Stops streaming and unblocks the reader; idempotent. */
    fun stop() {
        if (stopped) return
        stopped = true
        closeQuietly()
        thread?.interrupt()
        try {
            thread?.join(1000)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
        thread = null
        setStage("stopped", null)
    }

    private fun loop() {
        val http = HttpClient.newBuilder()
            .connectTimeout(Duration.ofMillis(timeoutMs))
            .build()
        var attempt = 0
        var blockedHere: ProtocolBlocked? = null
        while (!stopped) {
            if (attempt > 0) {
                val shift = if (attempt - 1 > 16) 16 else attempt - 1
                val backoff = minOf(maxBackoffMs, minBackoffMs shl shift) + random.nextInt(100)
                setStage("retrying", "reconnect in ${backoff}ms from cursor $cursorValue")
                try {
                    Thread.sleep(backoff)
                } catch (e: InterruptedException) {
                    Thread.currentThread().interrupt()
                    break
                }
                if (stopped) break
            }
            setStage(if (attempt == 0) "connecting" else "retrying", "from cursor $cursorValue")
            try {
                connectOnce(http)
                attempt = 1
                setStage("retrying", "stream ended at cursor $cursorValue")
            } catch (e: ProtocolBlocked) {
                // Durable corruption/incompatibility: reconnect would replay
                // the same offending sequence forever. Stable, no retry.
                blockedHere = e
                break
            } catch (e: Exception) {
                if (stopped) break
                attempt = minOf(attempt + 1, MAX_BACKOFF_ATTEMPT)
                onError(wrap(e))
            }
        }
        val block = blockedHere
        if (block != null) {
            blockedValue = block
            setStage(
                "blocked",
                "${block.kind} at cursor ${block.cursor}: ${block.detail} " +
                    "[recovery: ${block.recovery.joinToString(" / ")}]"
            )
            onBlocked(block)
        } else {
            setStage("stopped", null)
        }
    }

    /** Connects and pumps frames until the stream ends. */
    private fun connectOnce(http: HttpClient) {
        val url = baseUrl.trimEnd('/') +
            "/native/session/" + URLEncoder.encode(sessionId, "UTF-8") +
            "/events?after=" + cursorValue
        val request = HttpRequest.newBuilder(URI.create(url))
            .timeout(Duration.ofMillis(timeoutMs))
            .header("Authorization", "Bearer $bearerToken")
            .header("Accept", "text/event-stream")
            .header("Last-Event-ID", cursorValue.toString())
            .header("Cache-Control", "no-cache")
            .GET()
            .build()
        val response = http.send(request, HttpResponse.BodyHandlers.ofInputStream())
        if (response.statusCode() !in 200..299) {
            val detail = response.body().use { readErrorBody(it) }
            throw NativeProtocolException(
                "GET /native/session/{id}/events",
                "stream rejected with HTTP ${response.statusCode()}$detail"
            )
        }
        val body = response.body()
        activeBody = body
        setStage("open", "cursor $cursorValue")
        try {
            pump(body)
        } finally {
            closeQuietly()
        }
    }

    /**
     * Line-framed SSE pump with a hard per-line bound. A durable frame that
     * cannot be interpreted — oversized, unparseable, mismatched, cursorless
     * — throws [ProtocolBlocked] instead of being skipped or replayed.
     */
    private fun pump(body: InputStream) {
        val reader = InputStreamReader(body, Charsets.UTF_8)
        val line = StringBuilder()
        var lineOverlong = false
        var eventName: String? = null
        var frameId: Long? = null
        var idSeen = false
        var idUnreadable = false
        val data = StringBuilder()
        var frameOverlong = false
        while (!stopped) {
            val c = reader.read()
            if (c < 0) return
            if (c == '\r'.toInt()) continue
            if (c != '\n'.toInt()) {
                if (line.length < maxFrameBytes) {
                    line.append(c.toChar())
                } else {
                    lineOverlong = true
                }
                continue
            }
            val overlong = lineOverlong
            val text = if (overlong) "" else line.toString()
            val prefix = if (overlong) line.toString() else ""
            line.setLength(0)
            lineOverlong = false
            if (text.isEmpty() && !overlong) {
                dispatch(eventName, frameId, idSeen, idUnreadable, data.toString(), frameOverlong)
                eventName = null
                frameId = null
                idSeen = false
                idUnreadable = false
                data.setLength(0)
                frameOverlong = false
                continue
            }
            if (overlong) {
                if (prefix.startsWith(":")) {
                    // A hostile oversized comment is not durable content: it
                    // is reported loudly and skipped (it can never replay).
                    onError(
                        NativeProtocolException(
                            "sse comment",
                            "comment exceeded the $maxFrameBytes byte bound; skipped"
                        )
                    )
                } else {
                    frameOverlong = true
                    if (prefix.startsWith("id:")) idUnreadable = true
                }
                continue
            }
            if (text.startsWith(":")) continue
            when {
                text.startsWith("event:") -> eventName = text.substring(6).trim()
                text.startsWith("id:") -> {
                    idSeen = true
                    val parsed = text.substring(3).trim().toLongOrNull()
                    if (parsed != null && parsed >= 0) {
                        frameId = parsed
                    } else {
                        idUnreadable = true
                    }
                }
                text.startsWith("data:") -> {
                    val chunk = text.substring(5)
                        .let { if (it.startsWith(" ")) it.substring(1) else it }
                    // Strictly bounded: never let a hostile frame grow RAM.
                    val separator = if (data.isEmpty()) 0 else 1
                    if (data.length + separator + chunk.length > maxFrameBytes) {
                        frameOverlong = true
                    } else {
                        if (separator == 1) data.append('\n')
                        data.append(chunk)
                    }
                }
            }
        }
    }

    private fun dispatch(
        eventName: String?,
        frameId: Long?,
        idSeen: Boolean,
        idUnreadable: Boolean,
        raw: String,
        overlong: Boolean
    ) {
        if (overlong) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.OVERSIZED_FRAME,
                cursorValue,
                "a durable frame exceeds the $maxFrameBytes byte bound; refusing rather than " +
                    "skipping event ${frameId ?: eventName ?: "(unidentified)"}"
            )
        }
        if (raw.isEmpty()) return
        val parsed = try {
            JsonCodec.parse(raw)
        } catch (e: NativeProtocolException) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.MALFORMED_FRAME,
                cursorValue,
                "data is not valid JSON (frame ${frameId ?: eventName ?: "(unidentified)"})"
            )
        }
        val obj = parsed as? JsonValue.Obj
            ?: throw ProtocolBlocked(
                ProtocolBlocked.Kind.MALFORMED_FRAME,
                cursorValue,
                "data is not a JSON object (frame ${frameId ?: eventName ?: "(unidentified)"})"
            )
        val declared = (obj.fields["event"] as? JsonValue.Str)?.value
        if (eventName == "error" || declared == "error") {
            // The daemon's terminal durable failure frame: it is never a
            // normal event and never advances the cursor.
            val code = (obj.fields["code"] as? JsonValue.Str)?.value
            if (code == "journal_read_failed") {
                throw ProtocolBlocked(
                    ProtocolBlocked.Kind.JOURNAL_UNREADABLE,
                    cursorValue,
                    "the daemon reported journal_read_failed: the durable journal cannot be read"
                )
            }
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.UNSUPPORTED_VERSION,
                cursorValue,
                "the daemon reported an unsupported terminal error frame (code ${code ?: "absent"})"
            )
        }
        val schema = (obj.fields["schema"] as? JsonValue.Str)?.value
        if (schema != null && schema != SUPPORTED_FRAME_SCHEMA) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.UNSUPPORTED_VERSION,
                cursorValue,
                "frame schema $schema is not $SUPPORTED_FRAME_SCHEMA"
            )
        }
        val version = obj.fields["v"]
        if (version != null && !(version is JsonValue.Int64 && version.value == 1L)) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.UNSUPPORTED_VERSION,
                cursorValue,
                "frame version $version is not supported"
            )
        }
        if (declared != null && eventName != null && eventName != declared) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.DISCRIMINATOR_MISMATCH,
                cursorValue,
                "event field $eventName disagrees with data discriminator $declared"
            )
        }
        val tagged = declared ?: eventName
            ?: throw ProtocolBlocked(
                ProtocolBlocked.Kind.DISCRIMINATOR_MISMATCH,
                cursorValue,
                "frame carries no event discriminator"
            )
        if (tagged == "heartbeat") {
            if (frameId != null) cursorValue = maxOf(cursorValue, frameId)
            return
        }
        if (idUnreadable || (idSeen && frameId == null)) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.IMPOSSIBLE_CURSOR,
                cursorValue,
                "frame ($tagged) carries an unreadable id cursor"
            )
        }
        if (frameId == null) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.IMPOSSIBLE_CURSOR,
                cursorValue,
                "durable frame ($tagged) carries no id cursor; refusing rather than replaying forever"
            )
        }
        if (frameId < cursorValue) {
            throw ProtocolBlocked(
                ProtocolBlocked.Kind.IMPOSSIBLE_CURSOR,
                cursorValue,
                "frame id $frameId is behind the resume cursor; the durable sequence cannot move backwards"
            )
        }
        if (frameId == cursorValue) return
        cursorValue = frameId
        onEvent(NativeSseEvent(frameId, tagged, parsed))
    }

    private fun readErrorBody(stream: InputStream): String {
        val out = StringBuilder()
        val reader = InputStreamReader(stream, Charsets.UTF_8)
        try {
            while (out.length < MAX_ERROR_BODY_CHARS) {
                val c = reader.read()
                if (c < 0) break
                out.append(c.toChar())
            }
        } catch (e: IOException) {
            // Best effort only.
        }
        val snippet = out.toString().trim()
        return if (snippet.isEmpty()) "" else ": ${snippet.take(MAX_ERROR_BODY_CHARS)}"
    }

    private fun closeQuietly() {
        val body = activeBody
        activeBody = null
        if (body != null) {
            try {
                body.close()
            } catch (e: IOException) {
                // Already closed.
            }
        }
    }

    private fun setStage(value: String, detail: String?) {
        stage = value
        onStatus(value, detail)
    }

    private fun wrap(e: Exception): Exception =
        if (e is NativeProtocolException) e
        else NativeProtocolException("GET /native/session/{id}/events", e.message ?: e.javaClass.simpleName)
}
