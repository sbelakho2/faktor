// Live model output subscription for the Faktor Native Protocol v1
// (`GET /native/session/{id}/live/output?after=` — `crates/server/src/native/
// live_output.rs`), the daemon's bounded per-token broadcaster. This is the
// JetBrains twin of the VS Code `LiveOutputStream` in
// `apps/vscode/src/eventStream.ts`; the wire contract is identical:
//
//  - the FIRST frame of every connection is a `snapshot` handshake
//    (`instance`/`session`/`sequence`/`watermark`/`turn`/`parts`) — the exact
//    starting sequence watermark, so subscribe-before-submit cannot lose a
//    fast reply;
//  - `live` frames carry `messageId`/`kind`/`partId`/`turn`/`attempt`/
//    `sequence`/`revision`/UTF-8 BYTE `offset`/`content` (whole code points);
//  - `resync` is an EXPLICIT lag signal: the client fetches
//    `/native/session/{id}/live/snapshot` and reconciles (never silent loss);
//  - `turn` frames are the durable turn projection — the ONLY completion
//    authority (never a quiet stream);
//  - `heartbeat` / comment keep-alives are ignored.
//
// Rules:
//  - live frames are EPHEMERAL: a malformed/oversized frame is dropped and
//    reported, never terminal (the offset gap triggers a resync);
//  - frames from a FOREIGN server instance (a daemon restart) are dropped;
//    the new handshake snapshot is the authority;
//  - replayed sequences (`sequence <= cursor`) are dropped before delivery;
//  - every connection re-handshakes with a fresh snapshot, so a reconnect can
//    neither skip nor duplicate text;
//  - retryable HTTP statuses (408/425/429/5xx) reconnect with bounded
//    exponential backoff; every other refusal enters the stable
//    `unavailable` state (the journal stream keeps the product working);
//  - the stream runs on one daemon thread and `stop()` closes the body, so no
//    socket or thread is orphaned.
//
// The client holds at most ONE parsed frame at a time (bounded by
// [maxFrameBytes]); frames are delivered synchronously to the caller's
// bounded sink (the frontend EDT pump), so the client itself can never
// accumulate an unbounded queue.
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

/** The bounded active-turn projection carried by a snapshot. */
data class LiveTurn(val op: String, val status: String)

/** One authoritative part snapshot (`GET /live/snapshot` / handshake). */
data class LiveSnapshotPart(
    val partId: String,
    val messageId: Long,
    val kind: String,
    val turn: String?,
    val attempt: Int,
    val revision: Long,
    /** UTF-8 byte length of [text] == the next expected frame offset. */
    val offset: Long,
    val text: String,
    val durableBytes: Long,
    val durableRevision: Long,
    val liveTail: Boolean
)

/** The subscribe-before-submit handshake snapshot. */
data class LiveSnapshot(
    val instance: String,
    val session: Long,
    val sequence: Long,
    val watermark: Long,
    val turn: LiveTurn?,
    val parts: List<LiveSnapshotPart>
)

/** One live chunk delta (ephemeral). */
data class LiveChunkFrame(
    val instance: String,
    val session: Long,
    val messageId: Long,
    val kind: String,
    val partId: String,
    val turn: String?,
    val attempt: Int,
    val sequence: Long,
    val revision: Long,
    /** UTF-8 byte offset where [content] begins in the logical part. */
    val offset: Long,
    val content: String
)

/** The explicit lag signal: the client must resync against the snapshot. */
data class LiveResyncFrame(
    val instance: String,
    val session: Long,
    val reason: String,
    val dropped: Long
)

/** The durable turn projection changed (the completion authority). */
data class LiveTurnFrame(
    val instance: String,
    val session: Long,
    val turn: String?,
    val status: String?
)

/** One delivered live-output message. */
sealed class LiveOutputMessage {
    data class Snapshot(val snapshot: LiveSnapshot) : LiveOutputMessage()
    data class Chunk(val frame: LiveChunkFrame) : LiveOutputMessage()
    data class Resync(val frame: LiveResyncFrame) : LiveOutputMessage()
    data class Turn(val frame: LiveTurnFrame) : LiveOutputMessage()
}

/** One typed live-output protocol refusal (a hostile payload shape). */
class LiveOutputError(detail: String) : NativeProtocolException("live output", detail)

/** A non-retryable subscription refusal (route/credential/contract). */
class LiveOutputUnavailableError(detail: String) : Exception(detail)

/**
 * Strict parsers of the live-output payloads (mirror of the VS Code
 * `parseLiveSnapshot`/`parseLiveChunk`): a hostile shape is a typed
 * [LiveOutputError], never a silently wrong UI.
 */
object LivePayloads {
    private val INSTANCE = Regex("^[0-9a-f]{16}$")

    fun parseSnapshot(json: String): LiveSnapshot {
        val record = JsonCodec.parse(json) as? JsonValue.Obj
            ?: throw LiveOutputError("snapshot body is not a JSON object")
        return parseSnapshotObject(record)
    }

    fun parseSnapshotObject(record: JsonValue.Obj): LiveSnapshot {
        val instance = instance(record, "snapshot")
        val session = number(record, "session", "snapshot")
        val sequence = number(record, "sequence", "snapshot")
        val watermark = number(record, "watermark", "snapshot")
        val turn = when (val raw = record.fields["turn"]) {
            null, is JsonValue.Null -> null
            is JsonValue.Obj -> LiveTurn(
                op = string(raw, "op", "snapshot.turn"),
                status = string(raw, "status", "snapshot.turn")
            )
            else -> throw LiveOutputError("snapshot: turn is not an object")
        }
        val rawParts = record.fields["parts"] as? JsonValue.Arr
            ?: throw LiveOutputError("snapshot: parts is not an array")
        if (rawParts.items.size > 64) {
            throw LiveOutputError("snapshot: parts array exceeds the bound")
        }
        val parts = rawParts.items.mapIndexed { index, raw ->
            val part = raw as? JsonValue.Obj
                ?: throw LiveOutputError("snapshot.parts[$index] is not an object")
            val text = when (val value = part.fields["text"]) {
                is JsonValue.Str -> value.value
                else -> throw LiveOutputError("snapshot.parts[$index].text is not a string")
            }
            val attempt = number(part, "attempt", "snapshot.parts[$index]")
            if (attempt > Int.MAX_VALUE.toLong()) {
                throw LiveOutputError("snapshot.parts[$index].attempt exceeds Int range")
            }
            LiveSnapshotPart(
                partId = string(part, "partId", "snapshot.parts[$index]"),
                messageId = number(part, "messageId", "snapshot.parts[$index]"),
                kind = string(part, "kind", "snapshot.parts[$index]"),
                turn = (part.fields["turn"] as? JsonValue.Str)?.value,
                attempt = attempt.toInt(),
                revision = number(part, "revision", "snapshot.parts[$index]"),
                offset = number(part, "offset", "snapshot.parts[$index]"),
                text = text,
                durableBytes = number(part, "durableBytes", "snapshot.parts[$index]"),
                durableRevision = number(part, "durableRevision", "snapshot.parts[$index]"),
                liveTail = (part.fields["liveTail"] as? JsonValue.Bool)?.value == true
            )
        }
        return LiveSnapshot(instance, session, sequence, watermark, turn, parts)
    }

    fun parseChunk(record: JsonValue.Obj): LiveChunkFrame {
        val attempt = number(record, "attempt", "live")
        if (attempt > Int.MAX_VALUE.toLong()) {
            throw LiveOutputError("live: attempt exceeds Int range")
        }
        return LiveChunkFrame(
            instance = instance(record, "live"),
            session = number(record, "session", "live"),
            messageId = number(record, "messageId", "live"),
            kind = string(record, "kind", "live"),
            partId = string(record, "partId", "live"),
            turn = (record.fields["turn"] as? JsonValue.Str)?.value,
            attempt = attempt.toInt(),
            sequence = number(record, "sequence", "live"),
            revision = number(record, "revision", "live"),
            offset = number(record, "offset", "live"),
            content = (record.fields["content"] as? JsonValue.Str)?.value ?: ""
        )
    }

    fun parseResync(record: JsonValue.Obj): LiveResyncFrame = LiveResyncFrame(
        instance = instance(record, "resync"),
        session = number(record, "session", "resync"),
        reason = string(record, "reason", "resync"),
        dropped = number(record, "dropped", "resync")
    )

    fun parseTurn(record: JsonValue.Obj): LiveTurnFrame = LiveTurnFrame(
        instance = instance(record, "turn"),
        session = number(record, "session", "turn"),
        turn = (record.fields["turn"] as? JsonValue.Str)?.value,
        status = (record.fields["status"] as? JsonValue.Str)?.value
    )

    private fun instance(record: JsonValue.Obj, what: String): String {
        val value = string(record, "instance", what)
        if (!INSTANCE.matches(value)) throw LiveOutputError("$what: malformed instance id")
        return value
    }

    private fun string(record: JsonValue.Obj, key: String, what: String): String {
        val value = (record.fields[key] as? JsonValue.Str)?.value
        if (value == null || value.isEmpty() || value.length > 256) {
            throw LiveOutputError("$what: missing/invalid $key")
        }
        return value
    }

    private fun number(record: JsonValue.Obj, key: String, what: String): Long {
        val value = (record.fields[key] as? JsonValue.Int64)?.value
        if (value == null || value < 0) {
            throw LiveOutputError("$what: missing/invalid $key")
        }
        return value
    }
}

/**
 * A reconnecting live-output subscription for one session. Listeners are
 * invoked on the stream thread; the UI must marshal/coalesce them.
 *
 * [snapshotFetcher] is the authoritative resync read (production:
 * [NativeClient.liveSnapshot]); when absent the `resync` signal is still
 * delivered so the frontend can fetch it itself.
 */
class NativeLiveOutputStream(
    private val baseUrl: String,
    private val bearerToken: String,
    private val sessionId: String,
    cursor: Long = 0,
    private val snapshotFetcher: (() -> LiveSnapshot)? = null,
    private val maxFrameBytes: Int = DEFAULT_MAX_FRAME_BYTES,
    private val minBackoffMs: Long = 250L,
    private val maxBackoffMs: Long = 15_000L,
    private val timeoutMs: Long = DEFAULT_TIMEOUT_MS,
    private val onMessage: (LiveOutputMessage) -> Unit,
    private val onStatus: (String, String?) -> Unit = { _, _ -> },
    private val onError: (Exception) -> Unit = {}
) {
    companion object {
        const val DEFAULT_MAX_FRAME_BYTES = 1 shl 20
        const val DEFAULT_TIMEOUT_MS = 30_000L

        private const val MAX_BACKOFF_ATTEMPT = 20
        private const val MAX_ERROR_BODY_CHARS = 200

        fun forConnection(
            connection: BackendEndpoint,
            sessionId: String,
            snapshotClient: NativeClient? = null,
            onMessage: (LiveOutputMessage) -> Unit,
            onStatus: (String, String?) -> Unit = { _, _ -> },
            onError: (Exception) -> Unit = {}
        ): NativeLiveOutputStream = NativeLiveOutputStream(
            connection.baseUrl,
            connection.password,
            sessionId,
            snapshotFetcher = snapshotClient?.let { client -> { client.liveSnapshot(sessionId) } },
            onMessage = onMessage,
            onStatus = onStatus,
            onError = onError
        )
    }

    @Volatile
    private var cursorValue: Long = if (cursor < 0) 0 else cursor

    @Volatile
    private var instanceValue: String? = null

    @Volatile
    private var stopped = true

    @Volatile
    private var stage: String = "stopped"

    @Volatile
    private var activeBody: InputStream? = null

    private var thread: Thread? = null

    private val random = Random()

    val cursor: Long
        get() = cursorValue

    /** The server-instance id of the LAST handshake (null before one). */
    val instance: String?
        get() = instanceValue

    val status: String
        get() = stage

    fun start() {
        if (!stopped) return
        stopped = false
        val t = Thread({ loop() }, "faktor-live-$sessionId")
        t.isDaemon = true
        thread = t
        t.start()
    }

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
        while (!stopped) {
            if (attempt > 0) {
                val shift = if (attempt - 1 > 16) 16 else attempt - 1
                val backoff = minOf(maxBackoffMs, minBackoffMs shl shift) + random.nextInt(100)
                setStage("retrying", "reconnect in ${backoff}ms from sequence $cursorValue")
                try {
                    Thread.sleep(backoff)
                } catch (e: InterruptedException) {
                    Thread.currentThread().interrupt()
                    break
                }
                if (stopped) break
            }
            setStage(if (attempt == 0) "connecting" else "retrying")
            try {
                connectOnce(http)
                attempt = 1
                setStage("retrying", "stream ended")
            } catch (e: LiveOutputUnavailableError) {
                // A route/credential/contract failure cannot be fixed by
                // reconnecting: stop instead of spinning forever. The journal
                // stream and durable refreshes keep the product working.
                stopped = true
                closeQuietly()
                onError(e)
                setStage("unavailable", e.message)
                break
            } catch (e: Exception) {
                if (stopped) break
                attempt = minOf(attempt + 1, MAX_BACKOFF_ATTEMPT)
                onError(wrap(e))
            }
        }
        if (stage != "unavailable") setStage("stopped", null)
    }

    private fun connectOnce(http: HttpClient) {
        val url = baseUrl.trimEnd('/') +
            "/native/session/" + URLEncoder.encode(sessionId, "UTF-8") +
            "/live/output?after=" + cursorValue
        val request = HttpRequest.newBuilder(URI.create(url))
            .timeout(Duration.ofMillis(timeoutMs))
            .header("Authorization", "Bearer $bearerToken")
            .header("Accept", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .GET()
            .build()
        val response = http.send(request, HttpResponse.BodyHandlers.ofInputStream())
        if (response.statusCode() !in 200..299) {
            val status = response.statusCode()
            val detail = response.body().use { readErrorBody(it) }
            val label = "subscription rejected with HTTP $status$detail"
            if (!isRetryableStatus(status)) {
                throw LiveOutputUnavailableError("$label; not retrying")
            }
            throw NativeProtocolException("GET /native/session/{id}/live/output", label)
        }
        val body = response.body()
        activeBody = body
        setStage("open")
        try {
            pump(body)
        } finally {
            closeQuietly()
        }
    }

    /**
     * Line-framed SSE pump with a hard per-frame byte bound. Live frames are
     * ephemeral: an oversized frame is dropped with a reported error (the
     * resulting gap triggers the resync path), never terminal.
     */
    private fun pump(body: InputStream) {
        val reader = InputStreamReader(body, Charsets.UTF_8)
        val line = StringBuilder()
        var lineBytes = 0L
        var lineOverlong = false
        var eventName: String? = null
        val data = StringBuilder()
        var dataBytes = 0L
        var frameOverlong = false
        while (!stopped) {
            val c = reader.read()
            if (c < 0) return
            if (c == '\r'.toInt()) continue
            if (c != '\n'.toInt()) {
                if (!lineOverlong) {
                    val ch = c.toChar()
                    val extra = when {
                        ch.isLowSurrogate() &&
                            line.isNotEmpty() &&
                            line[line.length - 1].isHighSurrogate() -> 1
                        ch.code < 0x80 -> 1
                        ch.code < 0x800 -> 2
                        else -> 3
                    }
                    if (lineBytes + extra <= maxFrameBytes) {
                        line.append(ch)
                        lineBytes += extra
                    } else {
                        lineOverlong = true
                    }
                }
                continue
            }
            val text = if (lineOverlong) "" else line.toString()
            val overlong = lineOverlong
            line.setLength(0)
            lineBytes = 0L
            lineOverlong = false
            if (text.isEmpty() && !overlong) {
                dispatch(eventName, if (frameOverlong) null else data.toString(), frameOverlong)
                eventName = null
                data.setLength(0)
                dataBytes = 0L
                frameOverlong = false
                continue
            }
            if (overlong) {
                // A hostile oversized line inside a live frame: drop the
                // frame, never retain it. A comment cannot be distinguished
                // after truncation, so it is treated as frame content.
                if (!text.startsWith(":")) frameOverlong = true
                continue
            }
            if (text.startsWith(":")) continue
            when {
                text.startsWith("event:") -> eventName = text.substring(6).trim()
                text.startsWith("data:") -> {
                    val chunk = text.substring(5)
                        .let { if (it.startsWith(" ")) it.substring(1) else it }
                    val separator = if (data.isEmpty()) 0 else 1
                    val chunkBytes = utf8Length(chunk)
                    if (dataBytes + separator + chunkBytes > maxFrameBytes) {
                        frameOverlong = true
                    } else {
                        if (separator == 1) data.append('\n')
                        data.append(chunk)
                        dataBytes += separator + chunkBytes
                    }
                }
            }
        }
    }

    private fun dispatch(eventName: String?, raw: String?, overlong: Boolean) {
        if (overlong) {
            onError(LiveOutputError("a live frame exceeded the $maxFrameBytes byte bound; dropped"))
            return
        }
        if (raw == null || raw.isEmpty()) {
            if (eventName != null && eventName != "heartbeat") {
                onError(LiveOutputError("frame $eventName carries no data"))
            }
            return
        }
        if (eventName == "heartbeat") return
        if (eventName == null) {
            onError(LiveOutputError("frame carries no event name; dropping"))
            return
        }
        val parsed = try {
            JsonCodec.parse(raw)
        } catch (e: NativeProtocolException) {
            onError(LiveOutputError("frame $eventName data is not JSON; dropping"))
            return
        }
        val record = parsed as? JsonValue.Obj
            ?: run {
                onError(LiveOutputError("frame $eventName data is not an object; dropping"))
                return
            }
        try {
            when (eventName) {
                "snapshot" -> adoptSnapshot(LivePayloads.parseSnapshotObject(record))
                "live" -> {
                    val frame = LivePayloads.parseChunk(record)
                    if (instanceValue != null && frame.instance != instanceValue) {
                        // A previous daemon's lineage: drop; the handshake
                        // snapshot of THIS instance is the authority.
                        onError(LiveOutputError("live frame from a foreign server instance; dropping"))
                        return
                    }
                    if (frame.sequence <= cursorValue) {
                        // Already-seen sequence (replay): never redeliver.
                        return
                    }
                    cursorValue = frame.sequence
                    onMessage(LiveOutputMessage.Chunk(frame))
                }
                "resync" -> {
                    val frame = LivePayloads.parseResync(record)
                    onMessage(LiveOutputMessage.Resync(frame))
                    resyncFromAuthority()
                }
                "turn" -> onMessage(LiveOutputMessage.Turn(LivePayloads.parseTurn(record)))
                else -> onError(LiveOutputError("unknown live frame \"$eventName\"; dropping"))
            }
        } catch (e: Exception) {
            onError(wrap(e))
        }
    }

    private fun adoptSnapshot(snapshot: LiveSnapshot) {
        instanceValue = snapshot.instance
        cursorValue = snapshot.sequence
        onMessage(LiveOutputMessage.Snapshot(snapshot))
    }

    /** The explicit resync: fetch the authoritative snapshot and adopt it. */
    private fun resyncFromAuthority() {
        val fetcher = snapshotFetcher ?: return
        try {
            adoptSnapshot(fetcher())
        } catch (e: Exception) {
            onError(wrap(e))
        }
    }

    private fun utf8Length(text: CharSequence): Long {
        var bytes = 0L
        var i = 0
        while (i < text.length) {
            val c = text[i]
            bytes += when {
                c.code < 0x80 -> 1
                c.code < 0x800 -> 2
                c.isHighSurrogate() && i + 1 < text.length && text[i + 1].isLowSurrogate() -> {
                    i += 1
                    4
                }
                else -> 3
            }
            i += 1
        }
        return bytes
    }

    private fun isRetryableStatus(status: Int): Boolean =
        status == 408 || status == 425 || status == 429 || status in 500..599

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
            // Best effort.
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

    private fun setStage(value: String, detail: String? = null) {
        stage = value
        onStatus(value, detail)
    }

    private fun wrap(e: Exception): Exception =
        if (e is NativeProtocolException || e is LiveOutputUnavailableError) e
        else NativeProtocolException(
            "GET /native/session/{id}/live/output",
            e.message ?: e.javaClass.simpleName
        )
}
